//! Permission engine: enforces the 5-layer intersection.
//!
//! Layer order (first failure decides):
//!   1. Policy denial of action name
//!   2. Tool package has all permissions the action requires
//!   3. Runner is allowed to call this action (if caller has a runner)
//!   4. Interface is allowed to trigger this action (if caller has an interface)
//!   5. Policy denial of any required permission
//!   6. Action confirm flag -> NeedsConfirmation
//!
//! Default: **deny**. A subject that is not listed in grants gets no permissions
//! and no allowlist. Tool with no granted set cannot satisfy any action that
//! requires permissions.

use thiserror::Error;

use crate::grants::Grants;
use crate::model::{ActionMeta, Caller, ToolMeta};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    NeedsConfirmation { reason: String },
    Deny { layer: DenyLayer, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyLayer {
    Policy,
    Tool,
    Runner,
    Interface,
    Service,
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("unknown tool `{0}`")]
    UnknownTool(String),
}

pub struct Engine {
    grants: Grants,
}

impl Engine {
    pub fn new(grants: Grants) -> Self {
        Self { grants }
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// Check whether `caller` may execute `action` under `tool`.
    ///
    /// `tool` is the package metadata for the tool the action belongs to. If
    /// the action's `tool` field is `None` and `tool` is also `None`, the
    /// engine treats package perms as empty (only useful for actions that
    /// require no permissions, e.g. trivial built-ins).
    pub fn check(&self, caller: &Caller, tool: Option<&ToolMeta>, action: &ActionMeta) -> Decision {
        let policy = self.grants.policy();

        // 1. Policy denial on action name.
        if policy.denies_action(&action.name) {
            return Decision::Deny {
                layer: DenyLayer::Policy,
                reason: format!("policy denies action `{}`", action.name),
            };
        }

        // 2. Tool package covers action requirements.
        //
        // The ONLY source of "granted" is the grants file. A tool's own
        // manifest declares what it `requires` (what it asks for) — that is
        // never self-granting. If the user hasn't approved the tool in the
        // grants file, the tool has no permissions. Default-deny.
        if !action.requires.is_empty() {
            // Grants are keyed by tool NAME, not by a registered tool object.
            // Prefer the registered `ToolMeta` name, but fall back to the action's
            // own declared tool (its `tool.action` namespace) so an action with no
            // explicit `agentd.tool{...}` is still grantable via `[tool.<ns>]`. The
            // executor's denial reporter and "allow forever" persistence both key
            // off `action.tool`; binding here the same way keeps all three in sync
            // and avoids the `<unknown>` / ungrantable-action footgun.
            let tool_name = tool.map(|t| t.name.as_str()).or(action.tool.as_deref());
            let pkg_granted = tool_name
                .and_then(|n| self.grants.tool(n).map(|g| g.granted.clone()))
                .unwrap_or_else(crate::model::PermissionSet::empty);
            if !pkg_granted.covers_all(&action.requires) {
                let missing: Vec<String> = action
                    .requires
                    .iter()
                    .filter(|r| !pkg_granted.contains(r))
                    .map(|p| p.as_str().to_string())
                    .collect();
                let tool_name = tool_name.unwrap_or("<unknown>");
                return Decision::Deny {
                    layer: DenyLayer::Tool,
                    reason: format!(
                        "tool `{tool_name}` missing permissions: {}",
                        missing.join(", ")
                    ),
                };
            }
        }

        // 3. Runner allowlist (if a runner is identified).
        if let Some(runner_id) = &caller.runner {
            let r = self.grants.runner(runner_id.as_str()).ok_or(());
            match r {
                Ok(r) if r.allowed_actions.contains(&action.name) => {}
                _ => {
                    return Decision::Deny {
                        layer: DenyLayer::Runner,
                        reason: format!(
                            "runner `{}` not allowed to call `{}`",
                            runner_id.as_str(),
                            action.name
                        ),
                    };
                }
            }
        }

        // 3b. Service allowlist (if a service is identified). Services that
        //     dispatch actions via `agentd.context.tools.call(...)` are gated
        //     here. Empty `allowed_actions` = no constraint at this layer.
        if let Some(svc_id) = &caller.service {
            if let Some(svc) = self.grants.service(svc_id.as_str()) {
                if !svc.allowed_actions.is_empty() && !svc.allowed_actions.contains(&action.name) {
                    return Decision::Deny {
                        layer: DenyLayer::Service,
                        reason: format!(
                            "service `{}` not allowed to call `{}`",
                            svc_id.as_str(),
                            action.name
                        ),
                    };
                }
            } else {
                return Decision::Deny {
                    layer: DenyLayer::Service,
                    reason: format!("service `{}` not registered in grants", svc_id.as_str()),
                };
            }
        }

        // 4. Interface allowlist (if an interface is identified).
        if let Some(iface_id) = &caller.interface
            && let Some(iface) = self.grants.interface(iface_id.as_str())
            && !iface.allowed_actions.is_empty()
            && !iface.allowed_actions.contains(&action.name)
        {
            return Decision::Deny {
                layer: DenyLayer::Interface,
                reason: format!(
                    "interface `{}` not allowed to call `{}`",
                    iface_id.as_str(),
                    action.name
                ),
            };
        }
        // No interface grant entry => no constraint applied at this layer.
        // Interface gating is opt-in: if you want to gate Telegram, list it.

        // 5. Policy denial of any required permission.
        for req in action.requires.iter() {
            if policy.denies_permission(req) {
                return Decision::Deny {
                    layer: DenyLayer::Policy,
                    reason: format!("policy denies permission `{}`", req.as_str()),
                };
            }
        }

        // 6. Confirmation gating. An operator "allow forever" on a confirm
        //    action records it in `policy.auto_confirm`, promoting it to Allow.
        if action.confirm && !policy.auto_confirms(&action.name) {
            return Decision::NeedsConfirmation {
                reason: format!("action `{}` requires confirmation", action.name),
            };
        }

        Decision::Allow
    }
}

impl Decision {
    /// True if a connected approver may override this decision at runtime. Only
    /// a missing grant (Tool layer) and confirm gating are escalatable; policy
    /// denylist and runner/interface/service allowlists are explicit operator
    /// intent and stay hard-deny.
    pub fn is_escalatable(&self) -> bool {
        matches!(
            self,
            Decision::NeedsConfirmation { .. }
                | Decision::Deny {
                    layer: DenyLayer::Tool,
                    ..
                }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::{GrantsFile, InterfaceGrants, RunnerGrants, ToolGrants};
    use crate::model::{Caller, PermissionSet, Policy};

    fn engine_with(file: GrantsFile) -> Engine {
        Engine::new(Grants::from_file(file))
    }

    fn read_action() -> ActionMeta {
        ActionMeta {
            name: "calendar.list_events".into(),
            tool: Some("google_calendar".into()),
            requires: PermissionSet::from_iter(["calendar.read"]),
            confirm: false,
        }
    }
    fn write_action() -> ActionMeta {
        ActionMeta {
            name: "calendar.create_event".into(),
            tool: Some("google_calendar".into()),
            requires: PermissionSet::from_iter(["calendar.write"]),
            confirm: true,
        }
    }
    fn calendar_tool(_manifest_requires: &[&str]) -> ToolMeta {
        // The values passed here are informational. The engine ignores them.
        ToolMeta {
            name: "google_calendar".into(),
            requires: PermissionSet::from_iter(_manifest_requires.iter().copied()),
        }
    }

    #[test]
    fn allows_when_all_layers_pass() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.read", "calendar.write"]),
            },
        );
        let engine = engine_with(file);
        let caller = Caller::default();
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &read_action());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn denies_when_tool_lacks_permission() {
        let engine = engine_with(GrantsFile::default());
        let caller = Caller::default();
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &read_action());
        assert!(matches!(
            decision,
            Decision::Deny {
                layer: DenyLayer::Tool,
                ..
            }
        ));
    }

    #[test]
    fn grants_bind_by_action_tool_name_without_registered_tool_meta() {
        // An action declares `tool: Some("google_calendar")` via its namespace
        // but no `agentd.tool{...}` was registered, so the dispatcher passes
        // `tool = None`. The `[tool.google_calendar]` grant must still apply —
        // otherwise the action is permanently ungrantable and errors print
        // `<unknown>` instead of the real tool name.
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.read"]),
            },
        );
        let engine = engine_with(file);
        let decision = engine.check(&Caller::default(), None, &read_action());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn deny_names_the_tool_namespace_when_no_tool_meta() {
        // Without a registered ToolMeta the denial must still name the tool
        // (from the action namespace), never `<unknown>`.
        let engine = engine_with(GrantsFile::default());
        let decision = engine.check(&Caller::default(), None, &read_action());
        match decision {
            Decision::Deny { layer, reason } => {
                assert_eq!(layer, DenyLayer::Tool);
                assert!(
                    reason.contains("google_calendar") && !reason.contains("<unknown>"),
                    "reason should name the tool namespace, got: {reason}"
                );
            }
            other => panic!("expected Tool deny, got {other:?}"),
        }
    }

    #[test]
    fn tool_meta_granted_is_not_self_granting() {
        // Even if ToolMeta carries permissions (e.g. inherited from a Lua
        // manifest), the engine ignores them. Only the grants file confers
        // grants. Default-deny is non-negotiable.
        let engine = engine_with(GrantsFile::default());
        let caller = Caller::default();
        let tool = calendar_tool(&["calendar.read"]);
        let decision = engine.check(&caller, Some(&tool), &read_action());
        assert!(matches!(
            decision,
            Decision::Deny {
                layer: DenyLayer::Tool,
                ..
            }
        ));
    }

    #[test]
    fn denies_when_runner_not_allowlisted() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.read"]),
            },
        );
        let engine = engine_with(file);
        let caller = Caller::default().with_runner("backend_reviewer");
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &read_action());
        assert!(matches!(
            decision,
            Decision::Deny {
                layer: DenyLayer::Runner,
                ..
            }
        ));
    }

    #[test]
    fn allows_runner_when_action_is_on_runner_allowlist() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.read"]),
            },
        );
        let mut runner = RunnerGrants::default();
        runner.allowed_actions.insert("calendar.list_events".into());
        file.runner.insert("backend_reviewer".into(), runner);
        let engine = engine_with(file);
        let caller = Caller::default().with_runner("backend_reviewer");
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &read_action());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn denies_when_interface_explicitly_constrained() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.read", "calendar.write"]),
            },
        );
        let mut iface = InterfaceGrants::default();
        iface.allowed_actions.insert("calendar.list_events".into());
        file.interface.insert("telegram".into(), iface);
        let engine = engine_with(file);
        let caller = Caller::default().interface_for_test("telegram");
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &write_action());
        assert!(matches!(
            decision,
            Decision::Deny {
                layer: DenyLayer::Interface,
                ..
            }
        ));
    }

    #[test]
    fn interface_without_entry_does_not_constrain() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.read"]),
            },
        );
        let engine = engine_with(file);
        let caller = Caller::default().interface_for_test("http");
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &read_action());
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn policy_action_deny_short_circuits() {
        let mut policy = Policy::default();
        policy.deny_actions.insert("calendar.list_events".into());
        let file = GrantsFile {
            policy,
            ..Default::default()
        };
        let engine = engine_with(file);
        let caller = Caller::default();
        let decision = engine.check(
            &caller,
            Some(&calendar_tool(&["calendar.read"])),
            &read_action(),
        );
        assert!(matches!(
            decision,
            Decision::Deny {
                layer: DenyLayer::Policy,
                ..
            }
        ));
    }

    #[test]
    fn policy_permission_deny_overrides_grant() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.read"]),
            },
        );
        file.policy.deny_permissions = PermissionSet::from_iter(["calendar.read"]);
        let engine = engine_with(file);
        let caller = Caller::default();
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &read_action());
        assert!(matches!(
            decision,
            Decision::Deny {
                layer: DenyLayer::Policy,
                ..
            }
        ));
    }

    #[test]
    fn confirm_flag_emits_needs_confirmation_when_otherwise_allowed() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.write"]),
            },
        );
        let engine = engine_with(file);
        let caller = Caller::default();
        let decision = engine.check(&caller, Some(&calendar_tool(&[])), &write_action());
        assert!(matches!(decision, Decision::NeedsConfirmation { .. }));
    }

    #[test]
    fn auto_confirm_promotes_needs_confirmation_to_allow() {
        let mut file = GrantsFile::default();
        file.tool.insert(
            "google_calendar".into(),
            ToolGrants {
                granted: PermissionSet::from_iter(["calendar.write"]),
            },
        );
        file.policy
            .auto_confirm
            .insert("calendar.create_event".into());
        let engine = engine_with(file);
        let decision = engine.check(
            &Caller::default(),
            Some(&calendar_tool(&[])),
            &write_action(),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn escalatable_classification() {
        assert!(Decision::NeedsConfirmation { reason: "x".into() }.is_escalatable());
        assert!(
            Decision::Deny {
                layer: DenyLayer::Tool,
                reason: "x".into()
            }
            .is_escalatable()
        );
        assert!(
            !Decision::Deny {
                layer: DenyLayer::Policy,
                reason: "x".into()
            }
            .is_escalatable()
        );
        assert!(
            !Decision::Deny {
                layer: DenyLayer::Runner,
                reason: "x".into()
            }
            .is_escalatable()
        );
        assert!(!Decision::Allow.is_escalatable());
    }

    #[test]
    fn action_with_no_requires_skips_tool_layer() {
        let action = ActionMeta {
            name: "trivial.noop".into(),
            tool: None,
            requires: PermissionSet::empty(),
            confirm: false,
        };
        let engine = engine_with(GrantsFile::default());
        let decision = engine.check(&Caller::default(), None, &action);
        assert_eq!(decision, Decision::Allow);
    }

    // Local helper to keep tests readable.
    trait CallerExt {
        fn interface_for_test(self, s: &str) -> Self;
    }
    impl CallerExt for Caller {
        fn interface_for_test(self, s: &str) -> Self {
            let mut c = self;
            c.interface = Some(s.into());
            c
        }
    }
}

//! Executor — universal scheduler.
//!
//! Owns every registry the daemon spawns work against: actions (`Registry`),
//! runners, services, skills, and providers. Three entry points:
//!
//! - [`Executor::run_action`] — short-lived; permission engine gates, trace
//!   records, handler runs.
//! - [`Executor::run_runner`] — composes a runner's system prompt from its
//!   skills, then drives the tool-use loop: each tool call the model emits is
//!   dispatched back through the action path (so the permission engine fires)
//!   and fed back, until the model returns plain text or the turn ceiling hits.
//! - [`Executor::start_service`] / [`Executor::start_services`] — spawns
//!   long-running services as Tokio tasks, supervising their state in the
//!   service registry.
//!
//! Everything funnels through the same `Registry`-backed dispatch path so the
//! permission engine, the trace sink, and the `ActiveContext` propagation
//! work identically whether the caller is a one-shot HTTP request, an LLM
//! tool call, or a background Discord gateway loop.

use std::sync::Arc;
use std::time::Instant;

use agentd_ai::ProviderRegistry;
use agentd_permissions::{
    ActionMeta as PermActionMeta, Caller, Decision, Engine, PermissionSet,
    ToolMeta as PermToolMeta, engine::DenyLayer,
};
use agentd_runners::{RunnerError, RunnerOutcome, RunnerRegistry};
use agentd_services::{ServiceRegistry, ServiceState};
use agentd_skills::SkillRegistry;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, CallContext, Dispatcher, Registry, RegistryError, RunnerDispatcher,
};
use async_trait::async_trait;
use tokio::task::JoinHandle;

#[async_trait]
impl Dispatcher for Executor {
    async fn dispatch(
        &self,
        caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
        self.run(caller, call).await
    }

    async fn check_grants(
        &self,
        caller: Caller,
        tool: &str,
        required: PermissionSet,
    ) -> agentd_types::GrantDecision {
        // Mint a virtual ActionMeta scoped to `tool`. The engine runs
        // the full 5-layer check exactly like it would for any real
        // action: policy denylist → tool package grants cover required
        // → runner allowlist on action name → interface allowlist →
        // confirm. The caller (CodexAppServerProvider) passes
        // `tool = "codex"` and `action_name` it picks (e.g.
        // `codex.shell.exec`); user policy decides whether the runner
        // is allowed to use it.
        let tool_meta = PermToolMeta {
            name: tool.to_string(),
            requires: PermissionSet::empty(),
        };
        let action_meta = PermActionMeta {
            name: tool.to_string(),
            tool: Some(tool.to_string()),
            requires: required,
            confirm: false,
        };
        let decision = self
            .engine
            .load()
            .check(&caller, Some(&tool_meta), &action_meta);
        match decision {
            Decision::Allow => agentd_types::GrantDecision::Allow,
            Decision::Deny { .. } | Decision::NeedsConfirmation { .. } => {
                agentd_types::GrantDecision::Deny(
                    decision_to_error(
                        decision,
                        &action_meta,
                        Some(tool),
                        &caller,
                        &self.engine.load(),
                    )
                    .to_string(),
                )
            }
        }
    }
}

/// Outcome of escalating a denial to the approval broker.
enum Escalation {
    /// Proceed with the dispatch; `extra` carries any allow-once grants.
    Proceed { extra: PermissionSet },
    /// Reject with this error (broker said deny, or persistence failed).
    Reject(RegistryError),
}

/// Reconstruct the `RegistryError` a non-escalated decision produces, with
/// enough component context for users to fix grants.toml without guessing.
fn decision_to_error(
    decision: Decision,
    action_meta: &PermActionMeta,
    tool_name: Option<&str>,
    caller: &Caller,
    engine: &Engine,
) -> RegistryError {
    match decision {
        Decision::NeedsConfirmation { reason } => RegistryError::NeedsConfirmation(format!(
            "{reason} (caller {})\nfix: run `agentctl grants listen` and approve the request, or add `{}` to `[policy].auto_confirm` in grants.toml",
            caller_summary(caller),
            action_meta.name
        )),
        Decision::Deny { layer, reason } => RegistryError::Denied {
            layer: deny_layer_label(layer).to_string(),
            reason: denial_reason(layer, reason, action_meta, tool_name, caller, engine),
        },
        Decision::Allow => RegistryError::Invocation(
            "the permission check unexpectedly allowed this action while a denial was being reported — this is a bug in agentd, please report it".into(),
        ),
    }
}

fn denial_reason(
    layer: DenyLayer,
    raw_reason: String,
    action_meta: &PermActionMeta,
    tool_name: Option<&str>,
    caller: &Caller,
    engine: &Engine,
) -> String {
    let action = action_meta.name.as_str();
    let caller_text = caller_summary(caller);
    match layer {
        DenyLayer::Tool => {
            let tool = tool_name
                .or(action_meta.tool.as_deref())
                .unwrap_or("<unknown>");
            let missing = missing_tool_permissions(engine, tool_name, action_meta);
            let missing_text = if missing.is_empty() {
                "the action requirements".to_string()
            } else {
                backtick_list(&missing)
            };
            let fix = if missing.is_empty() {
                format!(
                    "add the required permissions to `{}` in grants.toml",
                    toml_table("tool", tool)
                )
            } else {
                format!(
                    "add to grants.toml:\n{}\ngranted = [{}]",
                    toml_table("tool", tool),
                    toml_array(&missing)
                )
            };
            format!(
                "action `{action}` needs permissions that tool `{tool}` has not been granted\nmissing: {missing_text}\ncaller: {caller_text}\nfix: {fix}"
            )
        }
        DenyLayer::Runner => {
            let runner = caller_runner_name(caller);
            let fix = runner
                .as_deref()
                .map(|runner| {
                    format!(
                        "add to grants.toml:\n{}\nallowed_actions = [{}]",
                        toml_table("runner", runner),
                        toml_array(&[action.to_string()])
                    )
                })
                .unwrap_or_else(|| raw_reason.clone());
            format!(
                "action `{action}` is not allowed for runner `{}`\ncaller: {caller_text}\nfix: {fix}",
                runner.unwrap_or_else(|| "<unknown>".to_string())
            )
        }
        DenyLayer::Interface => {
            let iface = caller
                .interface
                .as_ref()
                .map(|id| id.as_str().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "action `{action}` is not allowed for interface `{iface}`\ncaller: {caller_text}\nfix: add to grants.toml:\n{}\nallowed_actions = [{}]",
                toml_table("interface", &iface),
                toml_array(&[action.to_string()])
            )
        }
        DenyLayer::Service => {
            let service = caller
                .service
                .as_ref()
                .map(|id| id.as_str().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "action `{action}` is not allowed for service `{service}`\ncaller: {caller_text}\nfix: add to grants.toml:\n{}\nallowed_actions = [{}]",
                toml_table("service", &service),
                toml_array(&[action.to_string()])
            )
        }
        DenyLayer::Policy => {
            let fix = if raw_reason.contains("permission `") {
                "remove the denied permission from `[policy].deny_permissions` in grants.toml"
            } else {
                "remove the action from `[policy].deny_actions` in grants.toml"
            };
            format!(
                "action `{action}` is blocked by policy\nreason: {raw_reason}\ncaller: {caller_text}\nfix: {fix}"
            )
        }
    }
}

fn missing_tool_permissions(
    engine: &Engine,
    tool_name: Option<&str>,
    action_meta: &PermActionMeta,
) -> Vec<String> {
    let tool_name = tool_name.or(action_meta.tool.as_deref());
    let granted = tool_name
        .and_then(|t| engine.grants().tool(t).map(|g| g.granted.clone()))
        .unwrap_or_else(PermissionSet::empty);
    action_meta
        .requires
        .iter()
        .filter(|p| !granted.contains(p))
        .map(|p| p.as_str().to_string())
        .collect()
}

fn caller_runner_name(caller: &Caller) -> Option<String> {
    caller.runner.as_ref().map(|id| id.as_str().to_string())
}

fn deny_layer_label(layer: DenyLayer) -> &'static str {
    match layer {
        DenyLayer::Policy => "policy",
        DenyLayer::Tool => "tool",
        DenyLayer::Runner => "runner",
        DenyLayer::Interface => "interface",
        DenyLayer::Service => "service",
    }
}

fn caller_summary(caller: &Caller) -> String {
    let mut parts = Vec::new();
    if let Some(v) = &caller.runner {
        parts.push(format!("runner `{}`", v.as_str()));
    }
    if let Some(v) = &caller.interface {
        parts.push(format!("interface `{}`", v.as_str()));
    }
    if let Some(v) = &caller.service {
        parts.push(format!("service `{}`", v.as_str()));
    }
    if let Some(v) = &caller.session {
        parts.push(format!("session `{}`", v.as_str()));
    }
    if let Some(v) = &caller.user {
        parts.push(format!("user `{}`", v.as_str()));
    }
    if parts.is_empty() {
        "direct call".to_string()
    } else {
        parts.join(", ")
    }
}

fn backtick_list(items: &[String]) -> String {
    items
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_table(section: &str, name: &str) -> String {
    format!("[{section}.{}]", toml_key(name))
}

fn toml_key(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        toml_string(name)
    }
}

fn toml_array(items: &[String]) -> String {
    items
        .iter()
        .map(|s| toml_string(s))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Coerce a toml_edit item into a string array in place.
fn ensure_string_array(item: &mut toml_edit::Item) -> &mut toml_edit::Array {
    if !item.is_array() {
        *item = toml_edit::value(toml_edit::Array::new());
    }
    item.as_array_mut().expect("ensured array")
}

/// Append values not already present (string compare).
fn append_unique(arr: &mut toml_edit::Array, vals: &[String]) {
    let existing: std::collections::HashSet<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    for v in vals {
        if !existing.contains(v) {
            arr.push(v.as_str());
        }
    }
}

pub struct Executor {
    registry: Arc<dyn Registry>,
    trace: Arc<dyn TraceSink>,
    /// Held in an `ArcSwap` so an "allow forever" approval can hot-swap a
    /// freshly-reloaded engine without locking the hot `check` path.
    engine: arc_swap::ArcSwap<Engine>,
    runners: RunnerRegistry,
    services: ServiceRegistry,
    skills: SkillRegistry,
    providers: Arc<ProviderRegistry>,
    max_runner_turns: u32,
    /// Optional approval broker. `None` ⇒ escalatable denials reject
    /// immediately (the pre-approvals behavior).
    broker: Option<Arc<dyn agentd_types::ApprovalBroker>>,
    /// Path to `grants.toml`, needed to persist "allow forever" verdicts.
    grants_path: Option<std::path::PathBuf>,
    /// Rebuilds the engine from `grants.toml` (daemon injects the
    /// `load_grants_file → expand_grants → Engine::new` pipeline). Returns the
    /// fresh engine or an error string.
    reload_grants: Option<Arc<dyn Fn() -> Result<Engine, String> + Send + Sync>>,
    /// Monotonic id source for approval requests.
    approval_seq: std::sync::atomic::AtomicU64,
    /// Serializes concurrent "allow forever" read-modify-write + engine swap.
    forever_write_lock: tokio::sync::Mutex<()>,
}

impl Executor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<dyn Registry>,
        trace: Arc<dyn TraceSink>,
        engine: Arc<Engine>,
        runners: RunnerRegistry,
        services: ServiceRegistry,
        skills: SkillRegistry,
        providers: Arc<ProviderRegistry>,
    ) -> Self {
        Self {
            registry,
            trace,
            engine: arc_swap::ArcSwap::from(engine),
            runners,
            services,
            skills,
            providers,
            max_runner_turns: Self::DEFAULT_MAX_RUNNER_TURNS,
            broker: None,
            grants_path: None,
            reload_grants: None,
            approval_seq: std::sync::atomic::AtomicU64::new(1),
            forever_write_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Wire the approval broker. Without it, escalatable denials reject
    /// immediately.
    pub fn set_broker(&mut self, broker: Arc<dyn agentd_types::ApprovalBroker>) {
        self.broker = Some(broker);
    }

    /// Set the `grants.toml` path used to persist "allow forever" verdicts.
    pub fn set_grants_path(&mut self, path: std::path::PathBuf) {
        self.grants_path = Some(path);
    }

    /// Inject the engine-reload closure (daemon's grants-build pipeline).
    pub fn set_reload_grants(&mut self, f: Arc<dyn Fn() -> Result<Engine, String> + Send + Sync>) {
        self.reload_grants = Some(f);
    }

    /// Override the tool-use loop ceiling (see [`Executor::DEFAULT_MAX_RUNNER_TURNS`]).
    /// The daemon calls this with `runtime.max_turns` from `config.toml`.
    /// A value of 0 is clamped to 1 so a runner always gets at least one turn.
    pub fn set_max_runner_turns(&mut self, turns: u32) {
        self.max_runner_turns = turns.max(1);
    }

    pub fn registry(&self) -> &Arc<dyn Registry> {
        &self.registry
    }
    pub fn runners(&self) -> &RunnerRegistry {
        &self.runners
    }
    pub fn services(&self) -> &ServiceRegistry {
        &self.services
    }
    pub fn skills(&self) -> &SkillRegistry {
        &self.skills
    }
    pub fn providers(&self) -> &Arc<ProviderRegistry> {
        &self.providers
    }

    // ---------- actions ----------

    pub async fn run_action(
        &self,
        caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
        self.run(caller, call).await
    }

    /// Compatibility alias for the previous single-method API.
    pub async fn run(
        &self,
        caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
        let action_name = call.action.clone();
        let args = call.args.clone();
        let started = Instant::now();
        let execution = caller.execution_str();

        let action_meta = match self.registry.action_info(&action_name) {
            Some(info) => info_to_perm_action(&info),
            None => {
                let dur = started.elapsed().as_millis();
                let err = RegistryError::NotFound(action_name.clone());
                self.trace
                    .record(
                        TraceEvent::err(&action_name, args, dur, err.to_string())
                            .with_execution(execution.clone())
                            .with_kind("action"),
                    )
                    .await;
                return Err((err, dur));
            }
        };
        let tool_name = action_meta.tool.clone();
        let tool_meta = tool_name
            .as_ref()
            .and_then(|t| self.registry.tool_info(t))
            .map(|i| info_to_perm_tool(&i));

        // Required permissions are the union of the action's own `requires`
        // and its tool's declared `requires`. Fold the tool's into the action
        // meta so the engine check, the escalation request, and grant
        // persistence all operate on the same set.
        let mut action_meta = action_meta;
        if let Some(t) = &tool_meta {
            for p in t.requires.iter() {
                action_meta.requires.insert(p.clone());
            }
        }

        let decision = self
            .engine
            .load()
            .check(&caller, tool_meta.as_ref(), &action_meta);

        // Permissions granted to *this dispatch only* on top of the static
        // grants (populated by an "allow once" verdict).
        let mut extra_grants = PermissionSet::empty();
        if !matches!(decision, Decision::Allow) {
            let escalatable = decision.is_escalatable() && self.broker.is_some();
            if escalatable {
                match self
                    .escalate(decision, &action_meta, tool_name.as_deref(), &caller)
                    .await
                {
                    Escalation::Proceed { extra } => extra_grants = extra,
                    Escalation::Reject(err) => {
                        let dur = started.elapsed().as_millis();
                        self.trace
                            .record(
                                TraceEvent::err(&action_name, args, dur, err.to_string())
                                    .with_execution(execution.clone())
                                    .with_kind("action"),
                            )
                            .await;
                        return Err((err, dur));
                    }
                }
            } else {
                // Not escalatable (policy / runner / interface / service) or no
                // broker wired: reject with a diagnostic that names the
                // denied component and the grants.toml fix.
                let err = decision_to_error(
                    decision,
                    &action_meta,
                    tool_name.as_deref(),
                    &caller,
                    &self.engine.load(),
                );
                let dur = started.elapsed().as_millis();
                self.trace
                    .record(
                        TraceEvent::err(&action_name, args, dur, err.to_string())
                            .with_execution(execution.clone())
                            .with_kind("action"),
                    )
                    .await;
                return Err((err, dur));
            }
        }

        // Base grants are read AFTER escalation: an "allow forever" verdict has
        // already reloaded the engine, so the missing perm is now static here;
        // an "allow once" verdict instead carries it in `extra_grants`.
        let mut effective_grants = tool_name
            .as_ref()
            .and_then(|t| {
                self.engine
                    .load()
                    .grants()
                    .tool(t)
                    .map(|g| g.granted.clone())
            })
            .unwrap_or_else(PermissionSet::empty);
        for p in extra_grants.iter() {
            effective_grants.insert(p.clone());
        }

        // A runner-initiated call inherits the runner's cwd (the action's own
        // `cwd`, if any, overrides it registry-side). Non-runner callers inherit
        // nothing → the registry falls back to the workspace root.
        let inherited_cwd = caller
            .runner
            .as_ref()
            .and_then(|r| self.runners.get(r.as_str()))
            .and_then(|def| def.cwd);

        let ctx = CallContext {
            caller: caller.clone(),
            effective_grants,
            call_chain: vec![action_name.clone()],
            cwd: inherited_cwd,
        };

        let outcome = self.registry.call(ctx, call).await;
        let dur = started.elapsed().as_millis();
        match outcome {
            Ok(res) => {
                self.trace
                    .record(
                        TraceEvent::ok(&action_name, args, dur, res.value.clone())
                            .with_execution(execution.clone())
                            .with_kind("action"),
                    )
                    .await;
                Ok((res, dur))
            }
            Err(e) => {
                self.trace
                    .record(
                        TraceEvent::err(&action_name, args, dur, e.to_string())
                            .with_execution(execution.clone())
                            .with_kind("action"),
                    )
                    .await;
                Err((e, dur))
            }
        }
    }

    // ---------- approval escalation ----------

    /// Escalate a non-`Allow`, escalatable decision to the approval broker and
    /// translate the verdict into an [`Escalation`]. Only called when a broker
    /// is wired and `decision.is_escalatable()`.
    async fn escalate(
        &self,
        decision: Decision,
        action_meta: &PermActionMeta,
        tool_name: Option<&str>,
        caller: &Caller,
    ) -> Escalation {
        use std::sync::atomic::Ordering;

        let broker = self
            .broker
            .as_ref()
            .expect("escalate called without a broker");

        // Classify + compute the missing perms (Tool layer) using the same
        // wildcard-aware `contains` the engine uses, so we persist exactly what
        // the engine would require.
        let (kind, missing): (agentd_types::ApprovalKind, Vec<String>) = match &decision {
            Decision::NeedsConfirmation { .. } => (agentd_types::ApprovalKind::Confirm, Vec::new()),
            Decision::Deny { .. } => {
                let pkg_granted = tool_name
                    .and_then(|t| {
                        self.engine
                            .load()
                            .grants()
                            .tool(t)
                            .map(|g| g.granted.clone())
                    })
                    .unwrap_or_else(PermissionSet::empty);
                let missing = action_meta
                    .requires
                    .iter()
                    .filter(|r| !pkg_granted.contains(r))
                    .map(|p| p.as_str().to_string())
                    .collect();
                (agentd_types::ApprovalKind::MissingGrant, missing)
            }
            Decision::Allow => unreachable!("escalate called on Allow"),
        };
        let reason = match &decision {
            Decision::NeedsConfirmation { reason } => reason.clone(),
            Decision::Deny { reason, .. } => reason.clone(),
            Decision::Allow => String::new(),
        };

        let id = self.approval_seq.fetch_add(1, Ordering::Relaxed);
        let req = agentd_types::ApprovalRequest {
            id,
            kind,
            action: action_meta.name.clone(),
            tool: tool_name.map(|s| s.to_string()),
            requires: action_meta
                .requires
                .iter()
                .map(|p| p.as_str().to_string())
                .collect(),
            missing: missing.clone(),
            reason,
            caller: caller.clone(),
        };

        let verdict = broker.request(req).await;
        self.trace
            .record(
                TraceEvent::ok(
                    &action_meta.name,
                    serde_json::json!({ "request_id": id }),
                    0,
                    serde_json::json!({ "approval": format!("{verdict:?}") }),
                )
                .with_execution(caller.execution_str())
                .with_kind("approval"),
            )
            .await;

        match verdict {
            agentd_types::Verdict::Deny => Escalation::Reject(decision_to_error(
                decision,
                action_meta,
                tool_name,
                caller,
                &self.engine.load(),
            )),
            agentd_types::Verdict::AllowOnce => Escalation::Proceed {
                extra: PermissionSet::from_iter(missing),
            },
            agentd_types::Verdict::AllowForever => {
                // Persist + reload. If the persistence plumbing is absent,
                // degrade to allow-once rather than silently dropping the
                // approval. A genuine write/reload failure rejects.
                if self.grants_path.is_none() || self.reload_grants.is_none() {
                    tracing::warn!(
                        action = %action_meta.name,
                        "allow-forever requested but grants path/reload not wired; \
                         degrading to allow-once"
                    );
                    return Escalation::Proceed {
                        extra: PermissionSet::from_iter(missing),
                    };
                }
                match self
                    .apply_forever(kind, tool_name, &action_meta.name, &missing)
                    .await
                {
                    Ok(()) => Escalation::Proceed {
                        extra: PermissionSet::empty(),
                    },
                    Err(e) => Escalation::Reject(RegistryError::Invocation(format!(
                        "the approval was granted but could not be saved to grants.toml ({e}) — approve again or edit grants.toml by hand"
                    ))),
                }
            }
        }
    }

    /// Persist an "allow forever" verdict to `grants.toml` and hot-reload the
    /// engine. Serialized against concurrent writers via `forever_write_lock`.
    async fn apply_forever(
        &self,
        kind: agentd_types::ApprovalKind,
        tool_name: Option<&str>,
        action_name: &str,
        missing: &[String],
    ) -> Result<(), String> {
        let path = self.grants_path.as_ref().ok_or_else(|| {
            "the daemon has no grants file path configured, so approvals cannot be saved"
                .to_string()
        })?;
        let reload = self.reload_grants.as_ref().ok_or_else(|| {
            "the daemon has no grants reload hook wired, so approvals cannot be saved".to_string()
        })?;

        let _guard = self.forever_write_lock.lock().await;

        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| {
            format!("grants.toml is not valid TOML ({e}) — fix the syntax and approve again")
        })?;

        match kind {
            agentd_types::ApprovalKind::MissingGrant => {
                let tool =
                    tool_name.ok_or_else(|| {
                        "the approval did not name a tool, so there is nothing to grant the permissions to".to_string()
                    })?;
                let granted = doc
                    .as_table_mut()
                    .entry("tool")
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("the `tool` key in grants.toml is not a table — remove or rename the conflicting `tool` entry")?
                    .entry(tool)
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("this tool's entry in grants.toml is not a table — remove or rename the conflicting entry")?
                    .entry("granted")
                    .or_insert(toml_edit::value(toml_edit::Array::new()));
                append_unique(ensure_string_array(granted), missing);
            }
            agentd_types::ApprovalKind::Confirm => {
                let ac = doc
                    .as_table_mut()
                    .entry("policy")
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("the `policy` key in grants.toml is not a table — remove or rename the conflicting `policy` entry")?
                    .entry("auto_confirm")
                    .or_insert(toml_edit::value(toml_edit::Array::new()));
                append_unique(
                    ensure_string_array(ac),
                    std::slice::from_ref(&action_name.to_string()),
                );
            }
        }

        std::fs::write(path, doc.to_string()).map_err(|e| {
            format!("could not write grants.toml ({e}) — check the file permissions")
        })?;
        let fresh = reload()?;
        self.engine.store(Arc::new(fresh));
        Ok(())
    }

    // ---------- runners ----------

    /// Default ceiling on tool-use loop iterations. A misbehaving model that
    /// keeps emitting tool calls without ever returning text would otherwise
    /// run forever. Override the active ceiling via [`Executor::set_max_runner_turns`].
    pub const DEFAULT_MAX_RUNNER_TURNS: u32 = 16;

    /// Run a runner. For `LoopMode::ExecutorOwned` providers (Claude API,
    /// OpenAI API, Mock) this owns the agent loop:
    ///
    /// 1. Compose system prompt + tool catalog from the runner's skills and
    ///    `allowed_actions`.
    /// 2. Call `provider.complete(req)`.
    /// 3. If the response carries `tool_calls`, dispatch each through
    ///    `Executor::run` so the full permission engine fires, append a
    ///    `Role::Tool` message per call, and loop.
    /// 4. Stop when the provider returns a text-only reply or the
    ///    `max_turns` budget is exhausted.
    ///
    /// For `LoopMode::ProviderOwned` providers (Claude CLI w/ MCP, Codex CLI
    /// w/ MCP) the executor just configures `mcp_endpoint` and calls
    /// `complete` once — the provider already runs its own loop.
    pub async fn run_runner(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        prompt: String,
    ) -> Result<RunnerOutcome, RunnerError> {
        let def = self
            .runners
            .get(runner_name)
            .ok_or_else(|| RunnerError::NotFound(runner_name.to_string()))?;
        let composition = agentd_runners::compose(&def, &self.skills)?;

        let (provider_name, provider, model_id) = match composition.model.as_deref() {
            Some(m) => {
                self.providers
                    .resolve_for_model(m)
                    .ok_or_else(|| RunnerError::NoProvider {
                        name: def.name.clone(),
                        model: composition.model.clone(),
                    })?
            }
            None => {
                let (n, p) =
                    self.providers
                        .resolve(None)
                        .ok_or_else(|| RunnerError::NoProvider {
                            name: def.name.clone(),
                            model: None,
                        })?;
                (n, p, String::new())
            }
        };

        // Caller seen by every dispatched tool inherits the runner identity
        // so the permission engine's layer-3 check (`runner.allowed_actions`)
        // gates which actions this runner may invoke.
        let tool_caller = {
            let mut c = caller.clone();
            c.runner = Some(runner_name.into());
            c
        };

        // Build the tool catalog the model sees this turn. Source = runner's
        // composed `allowed_actions`; the registry supplies the action's
        // metadata for description + requires.
        let tools = self.build_tool_catalog(&composition.allowed_actions);

        let mut req = agentd_ai::CompletionRequest::default();
        if !composition.system.is_empty() {
            req.system = Some(composition.system.clone());
        }
        if !model_id.is_empty() {
            req.model = Some(model_id);
        }
        req.messages = vec![agentd_ai::Message::user(prompt)];
        req.tools = tools;

        let last_stop_reason;
        let final_text;

        let req_model_echo = req.model.clone();
        match provider.loop_mode() {
            agentd_ai::LoopMode::ProviderOwned => {
                // Spin a per-invocation MCP loopback. The provider (a CLI
                // wrapper) drives its own agent loop and reaches back into
                // this executor for every tool call via the loopback URL.
                // The handle drops at end-of-scope, killing the listener so
                // we don't leak ports across runner runs.
                let dispatcher: Arc<dyn Dispatcher> = self.clone();
                // A provider that bakes the token into a long-lived subprocess
                // (codex) dictates a stable token; header-based providers
                // (claude) get a fresh random one per invocation.
                let token = provider
                    .preferred_mcp_token()
                    .unwrap_or_else(agentd_mcp::gen_token);
                let loopback = agentd_mcp::bind_loopback(
                    dispatcher,
                    tool_caller.clone(),
                    req.tools.clone(),
                    token,
                )
                .await
                .map_err(|e| RunnerError::Provider {
                    provider: provider_name.clone(),
                    source: agentd_ai::ProviderError::Config(format!(
                        "could not start the local MCP bridge that lets the model call tools ({e})"
                    )),
                })?;
                req.mcp_endpoint = Some(agentd_ai::McpEndpoint::Http {
                    url: loopback.url.clone(),
                    token: loopback.token.clone(),
                });
                // Also wire the in-process dispatcher path. ProviderOwned
                // providers that can't use the HTTP loopback (codex
                // app-server speaks JSON-RPC over stdio, not MCP) read
                // these to bridge tool calls + approval requests through
                // the agentd permission engine.
                req.dispatcher = Some(self.clone());
                req.caller = Some(tool_caller.clone());
                let resp = provider
                    .complete(req)
                    .await
                    .map_err(|e| RunnerError::Provider {
                        provider: provider_name.clone(),
                        source: e,
                    })?;
                drop(loopback);
                final_text = resp.text;
                last_stop_reason = resp.stop_reason;
            }
            agentd_ai::LoopMode::ExecutorOwned => {
                let mut turns: u32 = 0;
                loop {
                    if turns >= self.max_runner_turns {
                        return Err(RunnerError::Provider {
                            provider: provider_name.clone(),
                            source: agentd_ai::ProviderError::Upstream(format!(
                                "the runner stopped after {} tool-use turns without producing a final answer — raise the turn limit or simplify the task",
                                self.max_runner_turns
                            )),
                        });
                    }
                    turns += 1;

                    let resp = provider.complete(req.clone()).await.map_err(|e| {
                        RunnerError::Provider {
                            provider: provider_name.clone(),
                            source: e,
                        }
                    })?;

                    if resp.tool_calls.is_empty() {
                        final_text = resp.text;
                        last_stop_reason = resp.stop_reason;
                        break;
                    }

                    // Record the assistant turn that asked for tools, then
                    // dispatch each call. Errors become tool_result strings
                    // rather than runner-level failures so the model can
                    // recover (the standard agent convention).
                    let assistant_text = resp.text.clone();
                    req.messages.push(agentd_ai::Message {
                        role: agentd_ai::Role::Assistant,
                        content: assistant_text,
                        tool_calls: resp.tool_calls.clone(),
                        tool_call_id: None,
                    });

                    for call in resp.tool_calls {
                        let call_id = call.id.clone();
                        let action = agentd_types::ActionCall {
                            action: call.name.clone(),
                            args: call.arguments.clone(),
                        };
                        let result_text = match self.run(tool_caller.clone(), action).await {
                            Ok((res, _)) => serde_json::to_string(&res.value).unwrap_or_else(|e| {
                                format!("the tool result could not be serialized to JSON ({e})")
                            }),
                            Err((e, _)) => format!("tool call failed ({e})"),
                        };
                        req.messages
                            .push(agentd_ai::Message::tool_result(call_id, result_text));
                    }
                }
            }
        }

        Ok(RunnerOutcome {
            text: final_text,
            provider: provider_name,
            model: req_model_echo,
            stop_reason: last_stop_reason,
        })
    }

    /// Translate runner.allowed_actions into `ToolDef`s the provider sees.
    /// Pulls each action's description + required-perms summary from the
    /// registry; unknown action names are silently skipped (don't expose a
    /// non-existent tool to the model).
    fn build_tool_catalog(&self, allowed: &[String]) -> Vec<agentd_ai::ToolDef> {
        let mut out = Vec::new();
        for name in allowed {
            let Some(info) = self.registry.action_info(name) else {
                continue;
            };
            let description = if info.requires.is_empty() {
                None
            } else {
                Some(format!("Requires: {}", info.requires.join(", ")))
            };
            out.push(agentd_ai::ToolDef {
                name: info.name,
                description,
                // Prefer the action's declared schema (compiled from its Lua
                // `input` table) so the model sees exact field names, types,
                // and required-ness. Actions without one fall back to a
                // free-form unconstrained object.
                input_schema: info
                    .input_schema
                    .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
            });
        }
        out
    }

    /// Spawn one service as a Tokio task and return its `JoinHandle`. The
    /// task drives the Lua body to completion (or until daemon shutdown).
    /// Honors the service's `restart` / `backoff_ms` policy: `"always"` and
    /// `"on_failure"` re-enter the body after exponential backoff; otherwise
    /// the task exits on the first clean return or error.
    pub fn start_service(&self, name: &str) -> Option<JoinHandle<()>> {
        let def = match self.services.get(name) {
            Some(d) => d,
            None => {
                tracing::warn!(service = name, "start_service: unknown service");
                return None;
            }
        };

        let registry = self.registry.clone();
        let services = self.services.clone();
        let trace = self.trace.clone();
        let granted = self
            .engine
            .load()
            .grants()
            .service(name)
            .map(|g| g.granted.clone())
            .unwrap_or_else(PermissionSet::empty);
        let svc_name = name.to_string();
        let restart = def.restart.clone();
        let backoff_ms = def.backoff_ms.unwrap_or(1000);
        let backoff_max_ms = def.backoff_max_ms.unwrap_or(60_000);

        services.set_state(&svc_name, ServiceState::Running);
        trace_lifecycle(&trace, &svc_name, "started").await_blocking();

        let handle = tokio::spawn(async move {
            let mut current_backoff = backoff_ms;
            loop {
                let ctx = CallContext {
                    caller: Caller::service(svc_name.clone()),
                    effective_grants: granted.clone(),
                    call_chain: vec![format!("service:{svc_name}")],
                    cwd: None,
                };
                let outcome = registry.call_service(ctx, &svc_name).await;
                let crashed = outcome.is_err();
                match &outcome {
                    Ok(()) => {
                        services.set_state(&svc_name, ServiceState::Stopped);
                        trace
                            .record(TraceEvent::ok(
                                &format!("service:{svc_name}"),
                                serde_json::Value::Null,
                                0,
                                serde_json::json!({ "lifecycle": "stopped" }),
                            ))
                            .await;
                        tracing::info!(service = %svc_name, "service stopped");
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        services.set_error(&svc_name, &msg);
                        trace
                            .record(TraceEvent::err(
                                &format!("service:{svc_name}"),
                                serde_json::Value::Null,
                                0,
                                msg.clone(),
                            ))
                            .await;
                        tracing::error!(service = %svc_name, error = %msg, "service crashed");
                    }
                }
                let should_restart = match restart.as_deref() {
                    Some("always") => true,
                    Some("on_failure") => crashed,
                    _ => false,
                };
                if !should_restart {
                    break;
                }
                if crashed {
                    tracing::warn!(
                        service = %svc_name, backoff_ms = current_backoff,
                        "supervised restart in {current_backoff}ms"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(current_backoff)).await;
                    current_backoff = (current_backoff.saturating_mul(2)).min(backoff_max_ms);
                } else {
                    // Clean exits reset backoff so a stable service that briefly
                    // returns doesn't get throttled.
                    current_backoff = backoff_ms;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                services.set_state(&svc_name, ServiceState::Running);
            }
        });
        Some(handle)
    }

    /// Spawn every registered service. Returns the join handles in name order
    /// so the daemon can await or supervise them.
    pub fn start_services(&self) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for name in self.services.names() {
            if let Some(h) = self.start_service(&name) {
                handles.push(h);
            }
        }
        handles
    }
}

// Tiny helper so the synchronous `start_service` body can still emit a
// lifecycle trace event without forcing the caller into async.
struct DeferredTrace<'a> {
    trace: &'a Arc<dyn TraceSink>,
    name: String,
    phase: &'static str,
}

impl DeferredTrace<'_> {
    fn await_blocking(self) {
        // Fire-and-forget: spawn the trace write so we don't block the caller.
        let trace = self.trace.clone();
        let name = self.name;
        let phase = self.phase;
        tokio::spawn(async move {
            trace
                .record(TraceEvent::ok(
                    &format!("service:{name}"),
                    serde_json::Value::Null,
                    0,
                    serde_json::json!({ "lifecycle": phase }),
                ))
                .await;
        });
    }
}

fn trace_lifecycle<'a>(
    trace: &'a Arc<dyn TraceSink>,
    name: &str,
    phase: &'static str,
) -> DeferredTrace<'a> {
    DeferredTrace {
        trace,
        name: name.to_string(),
        phase,
    }
}

fn info_to_perm_action(info: &agentd_types::RegistryActionInfo) -> PermActionMeta {
    PermActionMeta {
        name: info.name.clone(),
        tool: info.tool.clone(),
        requires: PermissionSet::from_iter(info.requires.iter().cloned()),
        confirm: info.confirm,
    }
}

fn info_to_perm_tool(info: &agentd_types::RegistryToolInfo) -> PermToolMeta {
    PermToolMeta {
        name: info.name.clone(),
        requires: PermissionSet::from_iter(info.requires.iter().cloned()),
    }
}

// ---------- RunnerDispatcher impl ----------
//
// Bridges Lua's `agentd.runners.run(name, opts)` into the executor's
// `run_runner_extended` pipeline. Opts come in as JSON:
//
//   { prompt?, messages?, history?, system?, model? }
//
// The dispatcher is a thin wrapper around `Arc<Executor>` because the
// underlying runner loop needs to clone `Arc<Self>` for the MCP loopback
// branch — a trait method can't bound that on a raw `&Executor` ref.

pub struct ExecutorHandle(pub Arc<Executor>);

impl ExecutorHandle {
    pub fn new(executor: Arc<Executor>) -> Arc<Self> {
        Arc::new(Self(executor))
    }
}

#[async_trait]
impl RunnerDispatcher for ExecutorHandle {
    async fn run_runner_json(
        &self,
        caller: Caller,
        name: &str,
        opts: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let prompt = opts
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let messages = opts
            .get("messages")
            .or_else(|| opts.get("history"))
            .cloned();
        let system_override = opts
            .get("system")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let model_override = opts
            .get("model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Time the runner run and emit a correlated trace event. Runner runs
        // were previously invisible in the trace — an action that fanned out
        // to N agents showed one event, hiding per-agent latency entirely.
        // The `execution` id (carried verbatim on the caller) ties this run
        // back to its parent action.
        let execution = caller.execution_str();
        let trace_action = format!("runner:{name}");
        let trace_args = serde_json::json!({ "runner": name });
        let started = Instant::now();
        let result = self
            .0
            .run_runner_extended(
                caller,
                name,
                prompt,
                messages,
                system_override,
                model_override,
            )
            .await;
        let dur = started.elapsed().as_millis();
        match result {
            Ok(out) => {
                self.0
                    .trace
                    .record(
                        TraceEvent::ok(
                            &trace_action,
                            trace_args,
                            dur,
                            serde_json::json!({
                                "model": out.model,
                                "provider": out.provider,
                                "stop_reason": out.stop_reason,
                                "chars": out.text.chars().count(),
                            }),
                        )
                        .with_execution(execution)
                        .with_kind("runner"),
                    )
                    .await;
                Ok(serde_json::json!({
                    "text": out.text,
                    "provider": out.provider,
                    "model": out.model,
                    "stop_reason": out.stop_reason,
                }))
            }
            Err(e) => {
                self.0
                    .trace
                    .record(
                        TraceEvent::err(
                            &trace_action,
                            serde_json::json!({ "runner": name }),
                            dur,
                            e.to_string(),
                        )
                        .with_execution(execution)
                        .with_kind("runner"),
                    )
                    .await;
                Err(e.to_string())
            }
        }
    }

    fn runner_names(&self) -> Vec<String> {
        self.0.runners.names()
    }
}

impl Executor {
    /// Run a runner with the full set of overrides Lua's
    /// `agentd.runners.run` accepts. Equivalent to `run_runner` for the
    /// simple-prompt case; extends it with explicit `messages` and per-call
    /// `system` / `model` overrides.
    pub async fn run_runner_extended(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        prompt: Option<String>,
        messages: Option<serde_json::Value>,
        system_override: Option<String>,
        model_override: Option<String>,
    ) -> Result<RunnerOutcome, RunnerError> {
        let def = self
            .runners
            .get(runner_name)
            .ok_or_else(|| RunnerError::NotFound(runner_name.to_string()))?;
        let composition = agentd_runners::compose(&def, &self.skills)?;

        let model_for_resolve = model_override.clone().or_else(|| composition.model.clone());
        let (provider_name, provider, model_id) = match model_for_resolve.as_deref() {
            Some(m) => {
                self.providers
                    .resolve_for_model(m)
                    .ok_or_else(|| RunnerError::NoProvider {
                        name: def.name.clone(),
                        model: model_for_resolve.clone(),
                    })?
            }
            None => {
                let (n, p) =
                    self.providers
                        .resolve(None)
                        .ok_or_else(|| RunnerError::NoProvider {
                            name: def.name.clone(),
                            model: None,
                        })?;
                (n, p, String::new())
            }
        };

        let tool_caller = {
            let mut c = caller.clone();
            c.runner = Some(runner_name.into());
            c
        };
        let tools = self.build_tool_catalog(&composition.allowed_actions);

        let mut composed_system = composition.system.clone();
        if let Some(extra) = system_override {
            let extra = extra.trim();
            if !extra.is_empty() {
                if composed_system.is_empty() {
                    composed_system = extra.to_string();
                } else {
                    composed_system.push_str("\n\n");
                    composed_system.push_str(extra);
                }
            }
        }

        let mut req = agentd_ai::CompletionRequest::default();
        if !composed_system.is_empty() {
            req.system = Some(composed_system);
        }
        if !model_id.is_empty() {
            req.model = Some(model_id);
        }
        if let Some(msgs) = messages {
            req.messages = parse_messages(msgs)?;
        }
        if let Some(p) = prompt
            && !p.is_empty()
        {
            req.messages.push(agentd_ai::Message::user(p));
        }
        if req.messages.is_empty() {
            return Err(RunnerError::Provider {
                provider: provider_name,
                source: agentd_ai::ProviderError::Config(
                    "`runners.run` needs something to send the model — pass a `prompt` string or a `messages` list".into(),
                ),
            });
        }
        req.tools = tools;

        let req_model_echo = req.model.clone();
        match provider.loop_mode() {
            agentd_ai::LoopMode::ProviderOwned => {
                let dispatcher: Arc<dyn Dispatcher> = self.clone();
                // A provider that bakes the token into a long-lived subprocess
                // (codex) dictates a stable token; header-based providers
                // (claude) get a fresh random one per invocation.
                let token = provider
                    .preferred_mcp_token()
                    .unwrap_or_else(agentd_mcp::gen_token);
                let loopback = agentd_mcp::bind_loopback(
                    dispatcher,
                    tool_caller.clone(),
                    req.tools.clone(),
                    token,
                )
                .await
                .map_err(|e| RunnerError::Provider {
                    provider: provider_name.clone(),
                    source: agentd_ai::ProviderError::Config(format!(
                        "could not start the local MCP bridge that lets the model call tools ({e})"
                    )),
                })?;
                req.mcp_endpoint = Some(agentd_ai::McpEndpoint::Http {
                    url: loopback.url.clone(),
                    token: loopback.token.clone(),
                });
                req.dispatcher = Some(self.clone());
                req.caller = Some(tool_caller.clone());
                let resp = provider
                    .complete(req)
                    .await
                    .map_err(|e| RunnerError::Provider {
                        provider: provider_name.clone(),
                        source: e,
                    })?;
                drop(loopback);
                Ok(RunnerOutcome {
                    text: resp.text,
                    provider: provider_name,
                    model: req_model_echo,
                    stop_reason: resp.stop_reason,
                })
            }
            agentd_ai::LoopMode::ExecutorOwned => {
                let mut turns = 0u32;
                loop {
                    if turns >= self.max_runner_turns {
                        return Err(RunnerError::Provider {
                            provider: provider_name,
                            source: agentd_ai::ProviderError::Upstream(format!(
                                "the runner stopped after {} tool-use turns without producing a final answer — raise the turn limit or simplify the task",
                                self.max_runner_turns
                            )),
                        });
                    }
                    turns += 1;
                    let resp = provider.complete(req.clone()).await.map_err(|e| {
                        RunnerError::Provider {
                            provider: provider_name.clone(),
                            source: e,
                        }
                    })?;
                    if resp.tool_calls.is_empty() {
                        return Ok(RunnerOutcome {
                            text: resp.text,
                            provider: provider_name,
                            model: req_model_echo,
                            stop_reason: resp.stop_reason,
                        });
                    }
                    req.messages.push(agentd_ai::Message {
                        role: agentd_ai::Role::Assistant,
                        content: resp.text.clone(),
                        tool_calls: resp.tool_calls.clone(),
                        tool_call_id: None,
                    });
                    for call in resp.tool_calls {
                        let id = call.id.clone();
                        let action = ActionCall {
                            action: call.name.clone(),
                            args: call.arguments.clone(),
                        };
                        let text = match self.run(tool_caller.clone(), action).await {
                            Ok((res, _)) => serde_json::to_string(&res.value).unwrap_or_else(|e| {
                                format!("the tool result could not be serialized to JSON ({e})")
                            }),
                            Err((e, _)) => format!("tool call failed ({e})"),
                        };
                        req.messages.push(agentd_ai::Message::tool_result(id, text));
                    }
                }
            }
        }
    }
}

fn parse_messages(v: serde_json::Value) -> Result<Vec<agentd_ai::Message>, RunnerError> {
    let arr = match v {
        serde_json::Value::Array(a) => a,
        _ => {
            return Err(RunnerError::Provider {
                provider: String::new(),
                source: agentd_ai::ProviderError::Config(
                    "the `messages` argument to `runners.run` must be an array of message tables"
                        .into(),
                ),
            });
        }
    };
    let mut out = Vec::with_capacity(arr.len());
    for (idx, item) in arr.into_iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| RunnerError::Provider {
            provider: String::new(),
            source: agentd_ai::ProviderError::Config(format!(
                "message {idx} in the `messages` list must be a table with `role` and `content` fields"
            )),
        })?;
        let role_s = obj
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("user")
            .to_string();
        let content = obj
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let role = match role_s.as_str() {
            "system" => agentd_ai::Role::System,
            "assistant" => agentd_ai::Role::Assistant,
            "tool" => agentd_ai::Role::Tool,
            _ => agentd_ai::Role::User,
        };
        out.push(agentd_ai::Message {
            role,
            content,
            tool_calls: Vec::new(),
            tool_call_id: None,
        });
    }
    Ok(out)
}

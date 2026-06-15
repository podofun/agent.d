use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Flat slug like `net:googleapis.com`. Holder-side slugs may use `*` / `**`
/// wildcards in the specifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Permission(pub String);

impl Permission {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Split into `(domain, specifier)`.
    pub fn parts(&self) -> (&str, Option<&str>) {
        match self.0.split_once(':') {
            Some((d, s)) => (d, Some(s)),
            None => (self.0.as_str(), None),
        }
    }

    /// Does this holder permission satisfy the given required permission?
    pub fn covers(&self, required: &Permission) -> bool {
        let (hd, hs) = self.parts();
        let (rd, rs) = required.parts();
        if hd != rd {
            return false;
        }
        match (hs, rs) {
            (None, None) => true,
            (None, Some(_)) | (Some(_), None) => false,
            (Some(h), Some(r)) => match_spec(h, r),
        }
    }
}

fn match_spec(holder: &str, required: &str) -> bool {
    if holder == "*" || holder == "**" {
        return true;
    }
    if holder == required {
        return true;
    }
    if let Some(prefix) = holder.strip_suffix("/**") {
        return required == prefix || required.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = holder.strip_suffix("/*") {
        if let Some(tail) = required.strip_prefix(&format!("{prefix}/")) {
            return !tail.contains('/');
        }
        return false;
    }
    if let Some(prefix) = holder.strip_suffix('*') {
        return required.starts_with(prefix);
    }
    false
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionSet(pub BTreeSet<Permission>);

impl<S: Into<String>> FromIterator<S> for PermissionSet {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        Self(iter.into_iter().map(Permission::new).collect())
    }
}

impl PermissionSet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, p: Permission) {
        self.0.insert(p);
    }

    pub fn contains(&self, p: &Permission) -> bool {
        self.0.iter().any(|h| h.covers(p))
    }

    /// All required permissions are covered by at least one holder.
    pub fn covers_all(&self, required: &PermissionSet) -> bool {
        required.0.iter().all(|r| self.contains(r))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Permission> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }
    };
}

string_id!(RunnerId);
string_id!(InterfaceId);
string_id!(ServiceId);
string_id!(SessionId);
string_id!(UserId);
string_id!(ExecutionId);

// ---------- Tool + action metadata ----------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolMeta {
    pub name: String,
    /// Permissions the tool package declares it WANTS (manifest). Never a grant
    /// by itself — the grants file is the only thing that confers grants.
    /// Engine uses this only for diagnostics / cross-checking.
    #[serde(default)]
    pub requires: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActionMeta {
    pub name: String,
    /// Tool this action belongs to (the part before the first `.` by convention).
    #[serde(default)]
    pub tool: Option<String>,
    /// Permissions this action requires to execute.
    #[serde(default)]
    pub requires: PermissionSet,
    /// Action requires per-call human confirmation (interface decides UX).
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Caller {
    pub runner: Option<RunnerId>,
    pub interface: Option<InterfaceId>,
    pub service: Option<ServiceId>,
    pub session: Option<SessionId>,
    pub user: Option<UserId>,
    /// Correlates every trace event produced while serving one top-level
    /// request. Minted at the interface boundary and carried verbatim into
    /// every child runner run + recursive action call, so a single scan and
    /// its N agent dispatches share one id in the trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<ExecutionId>,
}

impl Caller {
    pub fn interface(id: impl Into<InterfaceId>) -> Self {
        Self {
            interface: Some(id.into()),
            ..Default::default()
        }
    }
    pub fn service(id: impl Into<ServiceId>) -> Self {
        Self {
            service: Some(id.into()),
            ..Default::default()
        }
    }
    pub fn with_runner(mut self, r: impl Into<RunnerId>) -> Self {
        self.runner = Some(r.into());
        self
    }
    pub fn with_service(mut self, s: impl Into<ServiceId>) -> Self {
        self.service = Some(s.into());
        self
    }
    pub fn with_user(mut self, u: impl Into<UserId>) -> Self {
        self.user = Some(u.into());
        self
    }
    pub fn with_session(mut self, s: impl Into<SessionId>) -> Self {
        self.session = Some(s.into());
        self
    }
    pub fn with_execution(mut self, e: impl Into<ExecutionId>) -> Self {
        self.execution = Some(e.into());
        self
    }
    /// The execution id as a plain string, for trace stamping.
    pub fn execution_str(&self) -> Option<String> {
        self.execution.as_ref().map(|e| e.as_str().to_string())
    }
}

/// Context-level policy: blanket denies that override grants.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Action names this policy denies outright.
    #[serde(default)]
    pub deny_actions: BTreeSet<String>,
    /// Permissions this policy denies regardless of grant.
    #[serde(default)]
    pub deny_permissions: PermissionSet,
    /// Action names whose `confirm = true` gate is pre-approved. Set by an
    /// operator "allow forever" on a confirm action; promotes that action's
    /// `NeedsConfirmation` to `Allow`.
    #[serde(default)]
    pub auto_confirm: BTreeSet<String>,
}

impl Policy {
    pub fn denies_action(&self, name: &str) -> bool {
        self.deny_actions.contains(name)
    }
    pub fn denies_permission(&self, p: &Permission) -> bool {
        self.deny_permissions.contains(p)
    }
    pub fn auto_confirms(&self, name: &str) -> bool {
        self.auto_confirm.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_covers_exact() {
        assert!(Permission::new("calendar.read").covers(&Permission::new("calendar.read")));
        assert!(!Permission::new("calendar.read").covers(&Permission::new("calendar.write")));
    }

    #[test]
    fn permission_wildcard() {
        assert!(Permission::new("net:*").covers(&Permission::new("net:googleapis.com")));
        assert!(
            Permission::new("fs.read:/tmp/**").covers(&Permission::new("fs.read:/tmp/foo/bar"))
        );
        assert!(!Permission::new("fs.read:/tmp/**").covers(&Permission::new("fs.read:/other")));
        assert!(Permission::new("fs.read:/tmp/*").covers(&Permission::new("fs.read:/tmp/foo")));
        assert!(
            !Permission::new("fs.read:/tmp/*").covers(&Permission::new("fs.read:/tmp/foo/bar"))
        );
    }

    #[test]
    fn permission_domain_mismatch_never_covers() {
        assert!(!Permission::new("calendar.read").covers(&Permission::new("net:googleapis.com")));
    }

    #[test]
    fn set_covers_all() {
        let holder = PermissionSet::from_iter(["calendar.read", "calendar.write"]);
        let need = PermissionSet::from_iter(["calendar.write"]);
        assert!(holder.covers_all(&need));
        let bigger = PermissionSet::from_iter(["calendar.write", "calendar.delete"]);
        assert!(!holder.covers_all(&bigger));
    }

    #[test]
    fn empty_required_always_satisfied() {
        let holder = PermissionSet::empty();
        assert!(holder.covers_all(&PermissionSet::empty()));
    }

    #[test]
    fn shell_unrestricted_is_plain_slug() {
        // The sandbox escape hatch is an ordinary, non-wildcard grant; its
        // meaning (skip the native sandbox) lives in the shell binding, not the
        // permission engine.
        let held = Permission::new("shell.unrestricted");
        assert!(held.covers(&Permission::new("shell.unrestricted")));
        assert!(!held.covers(&Permission::new("shell.exec")));
    }
}

//! Approval DTOs + broker trait. Transport-agnostic: shared by the executor
//! (caller), `agentd-approvals` (impl), and `agentd-api` (control transport).
//!
//! An *escalatable* permission denial (a missing grant or a `confirm = true`
//! gate) is turned into an [`ApprovalRequest`] and handed to an
//! [`ApprovalBroker`]. The broker fans it out to whatever operator clients are
//! connected (agentctl `/control`, a future web UI, …) and returns the chosen
//! [`Verdict`]. No client is privileged by *being* a particular binary — the
//! trust boundary is the control channel the broker is wired to.

use agentd_permissions::{Caller, PermissionSet};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Why a request was escalated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// The tool lacks one or more required permissions in `grants.toml`.
    MissingGrant,
    /// The action is flagged `confirm = true`.
    Confirm,
    /// A runner tried an action absent from its `allowed_actions` allowlist.
    /// `AllowForever` appends the action to the runner's list.
    RunnerAction,
    /// A chat session asked to work in a folder the user has not trusted.
    /// `action` holds the folder; `AllowForever` adds it to `[folders].trusted`.
    Folder,
}

/// An operator's decision on an [`ApprovalRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Permit this single in-flight dispatch only; persist nothing.
    AllowOnce,
    /// Persist the grant to `grants.toml` and hot-reload the engine.
    AllowForever,
    /// Reject (same effect as the original denial).
    Deny,
}

/// One escalated permission decision awaiting an operator verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// Broker-assigned correlation id; echoed back in the resolve.
    pub id: u64,
    pub kind: ApprovalKind,
    pub action: String,
    pub tool: Option<String>,
    /// The action's full required permission set.
    pub requires: Vec<String>,
    /// For [`ApprovalKind::MissingGrant`]: the subset not yet granted (what
    /// `AllowForever` appends). Empty for [`ApprovalKind::Confirm`].
    pub missing: Vec<String>,
    pub reason: String,
    /// Identity of the invocation that tripped the denial.
    pub caller: Caller,
    /// What is actually being done: the action's arguments, a shell call's
    /// `argv` and `cwd`, a file's resolved path, or a host. Secrets read in
    /// the run are replaced by their names, and long values are capped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// The run the question belongs to, so approvers can tell windows apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// The durable session the run belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Shown before an approver writes "allow forever", for example on an
    /// interpreter, which would grant arbitrary execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// Transport-agnostic approver. Implemented by `agentd-approvals::Broker`.
#[async_trait]
pub trait ApprovalBroker: Send + Sync {
    /// Ask a connected approver to decide `req`. Returns [`Verdict::Deny`] if no
    /// approver is connected or the request times out (fail closed).
    async fn request(&self, req: ApprovalRequest) -> Verdict;
}

/// A capability check that failed *inside* a running handler — `ctx.fs`,
/// `ctx.http`, `ctx.secret`, … derived a permission slug the executing
/// tool/service has not been granted. Unlike the dispatch-time check, the
/// scheduler surfaces these mid-execution, so they carry the grant subject
/// and the call chain instead of a resolved action/tool pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InlineApprovalRequest {
    /// `"tool"` or `"service"` — which grants.toml table owns the execution.
    pub grant_kind: Option<String>,
    /// The `[tool.<name>]` / `[service.<name>]` grants entry to extend.
    pub grant_name: Option<String>,
    /// Action/service call chain, outermost first (e.g. `["git.status"]`).
    pub call_chain: Vec<String>,
    /// The single permission slug the handler needs (e.g. `fs.read:/tmp/x`).
    pub permission: String,
    pub caller: Caller,
    /// What the handler is doing, unredacted: a shell call's `argv` and
    /// `cwd`, for example. The executor redacts it before anyone sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Escalation point for [`InlineApprovalRequest`]s. Implemented by the
/// executor, which owns the broker, the trace sink, and the grants.toml
/// persistence used by an `AllowForever` verdict.
#[async_trait]
pub trait InlineApprovals: Send + Sync {
    /// Escalate an inline capability denial. Returns [`Verdict::Deny`] when no
    /// broker/approver is available or the operator rejects (fail closed).
    async fn request_inline(&self, req: InlineApprovalRequest) -> Verdict;

    /// Whether the run already allowed `permission` for the rest of the run
    /// (files and hosts only). Such allows never join a handler's grants.
    fn run_allows(&self, _run_id: &str, _permission: &str) -> bool {
        false
    }

    /// Note that the run read `value` from the secret `name`, so it is
    /// replaced by the name wherever it appears in a question.
    fn record_secret(&self, _run_id: &str, _name: &str, _value: &str) {}

    /// Ask like [`InlineApprovals::request_inline`], also returning the
    /// sentence explaining a refusal by someone other than a human (for
    /// example the classifier's reason), to put ahead of the denial.
    async fn request_inline_explained(&self, req: InlineApprovalRequest) -> InlineAnswer {
        InlineAnswer {
            verdict: self.request_inline(req).await,
            refusal: None,
        }
    }

    /// Whether `[approval].classifier` names a runner, so a session may be
    /// set to `classifier`.
    fn classifier_configured(&self) -> bool {
        false
    }
}

/// An inline question's answer, with who refused it and why when that was
/// not a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineAnswer {
    pub verdict: Verdict,
    pub refusal: Option<String>,
}

/// The operator's hard denials (`[denial].actions` and
/// `[denial].permissions`), read at the moment of each call so a reload applies
/// at once. A denied call is refused even when a grant covers it, and is never
/// offered for approval. Implemented by the executor. The denied permissions
/// are handed over as a set, so the caller can resolve path denials the same
/// way it resolves the paths it checks.
pub trait Denials: Send + Sync {
    fn denied_permissions(&self) -> PermissionSet;
    fn denies_action(&self, name: &str) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_serde_roundtrips_snake_case() {
        let j = serde_json::to_string(&Verdict::AllowForever).unwrap();
        assert_eq!(j, "\"allow_forever\"");
        let v: Verdict = serde_json::from_str("\"deny\"").unwrap();
        assert_eq!(v, Verdict::Deny);
    }

    #[test]
    fn kind_serde_snake_case() {
        let j = serde_json::to_string(&ApprovalKind::MissingGrant).unwrap();
        assert_eq!(j, "\"missing_grant\"");
    }
}

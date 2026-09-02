//! Approval DTOs + broker trait. Transport-agnostic: shared by the executor
//! (caller), `agentd-approvals` (impl), and `agentd-api` (control transport).
//!
//! An *escalatable* permission denial (a missing grant or a `confirm = true`
//! gate) is turned into an [`ApprovalRequest`] and handed to an
//! [`ApprovalBroker`]. The broker fans it out to whatever operator clients are
//! connected (agentctl `/control`, a future web UI, …) and returns the chosen
//! [`Verdict`]. No client is privileged by *being* a particular binary — the
//! trust boundary is the control channel the broker is wired to.

use agentd_permissions::Caller;
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
}

/// Escalation point for [`InlineApprovalRequest`]s. Implemented by the
/// executor, which owns the broker, the trace sink, and the grants.toml
/// persistence used by an `AllowForever` verdict.
#[async_trait]
pub trait InlineApprovals: Send + Sync {
    /// Escalate an inline capability denial. Returns [`Verdict::Deny`] when no
    /// broker/approver is available or the operator rejects (fail closed).
    async fn request_inline(&self, req: InlineApprovalRequest) -> Verdict;
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

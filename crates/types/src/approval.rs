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

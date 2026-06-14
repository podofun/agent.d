//! Hand-coded subset of the codex app-server protocol. We only model the
//! request/response shapes we actually send + the notifications we read.
//!
//! Schema reference: `codex app-server generate-json-schema --out <dir>`.
//! Tested against codex CLI v0.130. The schema is versioned (`v1` vs
//! `v2`); we use the v2

use serde::{Deserialize, Serialize};
use serde_json::Value;

// requests we issue

#[derive(Serialize, Debug, Clone)]
pub struct InitializeParams {
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

#[derive(Serialize, Debug, Clone)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

/// Parameters for `thread/start`. We only thread through the fields we
/// actively want to control. Everything else stays codex-default.
#[derive(Serialize, Debug, Clone, Default)]
pub struct ThreadStartParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `"read-only"` | `"workspace-write"` | `"danger-full-access"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// `"on-request"` | `"never"` | etc.
    #[serde(rename = "approvalPolicy", skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
    /// Codex will not persist the thread to disk if true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(rename = "baseInstructions", skip_serializing_if = "Option::is_none")]
    pub base_instructions: Option<String>,
    #[serde(
        rename = "developerInstructions",
        skip_serializing_if = "Option::is_none"
    )]
    pub developer_instructions: Option<String>,
    /// Free-form `config.toml`-shaped overrides. Codex accepts arbitrary
    /// keys here, so we can wedge MCP server entries (e.g. point
    /// `mcp_servers.agentd.url` at our loopback) without touching the
    /// user's `~/.codex/config.toml`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

#[derive(Serialize, Debug, Clone)]
pub struct TurnStartParams {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub input: Vec<UserInput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// agent tasks; the provider passes `"low"` by default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserInput {
    Text { text: String },
}

impl UserInput {
    pub fn text(s: impl Into<String>) -> Self {
        Self::Text { text: s.into() }
    }
}

// approval reply shapes

/// Reply body for `item/commandExecution/requestApproval`,
/// `item/fileChange/requestApproval`, etc. Values per codex schema:
/// `"approved"`, `"approved_for_session"`, `"denied"`, `"abort"`.
#[derive(Serialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approved,
    ApprovedForSession,
    Denied,
    Abort,
}

#[derive(Serialize, Debug, Clone)]
pub struct ReviewDecisionResp {
    pub decision: ReviewDecision,
}

// notification payloads we care about

/// Wrapper used by most `item/*` notifications. The discriminant lives on
/// `item.type`; we keep the rest as raw `Value` since we don't need to
/// statically model every codex item variant.
#[derive(Deserialize, Debug, Clone)]
pub struct ItemEvent {
    pub item: Item,
    #[serde(rename = "threadId")]
    pub thread_id: Option<String>,
    #[serde(rename = "turnId")]
    pub turn_id: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Item {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Option<String>,
    pub text: Option<String>,
    pub phase: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AgentMessageDelta {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "turnId")]
    pub turn_id: String,
    #[serde(rename = "itemId")]
    pub item_id: String,
    pub delta: String,
}

/// Minimal projection of the `thread/start` response.
#[derive(Deserialize, Debug, Clone)]
pub struct ThreadStartResult {
    pub thread: ThreadInfo,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ThreadInfo {
    pub id: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TurnStartResult {
    pub turn: TurnInfo,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TurnInfo {
    pub id: String,
}

/// `turn/completed` notification body.
#[derive(Deserialize, Debug, Clone)]
pub struct TurnCompleted {
    #[serde(rename = "threadId")]
    pub thread_id: String,
    pub turn: TurnSummary,
}

#[derive(Deserialize, Debug, Clone)]
pub struct TurnSummary {
    pub id: String,
    pub status: String,
    pub error: Option<Value>,
}

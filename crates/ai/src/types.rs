use std::sync::Arc;

use agentd_permissions::Caller;
use agentd_types::Dispatcher;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    /// `Role::Tool` carries `tool_result` payloads back into the conversation.
    /// Most providers represent tool results inside a user-role message in
    /// their native API, but keeping a distinct role here lets the executor
    /// and providers translate uniformly.
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls the assistant emitted as part of this message. Empty
    /// for plain text turns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// When `role == Tool`, references which `ToolCall.id` this message is
    /// answering. Ignored otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// Tool catalog entry the executor hands to a provider. The provider is
/// responsible for translating this into whatever its wire format expects
/// (Anthropic `tools`, OpenAI `tools`, Codex MCP config, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Stable id (matches `agentd.action{name=...}` for in-process tools).
    pub name: String,
    /// Human-readable summary the LLM uses to decide when to call.
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool's argument shape.
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// One concrete invocation the model wants to execute. Producers fill `id`
/// with whatever opaque token they got from the upstream API; consumers
/// must echo it back in the matching `Role::Tool` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// Where a provider can reach the executor's MCP loopback. Only used by
/// `LoopMode::ProviderOwned` providers (CLI shells); ignored otherwise.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase", tag = "kind")]
pub enum McpEndpoint {
    /// Spawn a child process — args derived from the provider's own
    /// requirements — and the MCP server's stdio is wired to it.
    Stdio { command: String, args: Vec<String> },
    /// Connect over HTTP. Useful for sandboxed CLIs that can't share a pipe.
    /// `token` is the bearer the loopback requires on every request — the
    /// provider must forward it in its MCP client config.
    Http { url: String, token: String },
}

/// How the provider participates in the tool-use loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    /// Provider returns assistant text plus `tool_calls`. Executor dispatches
    /// each call via its own engine, appends `Role::Tool` results, and calls
    /// `complete` again until no more tool_calls. API providers (Anthropic
    /// API, OpenAI API) take this path.
    ExecutorOwned,
    /// Provider drives the loop internally — it spawns a CLI that already
    /// runs an agent loop and reaches back into the executor via MCP. The
    /// returned `CompletionResponse.tool_calls` MUST be empty; `text` is the
    /// final assistant message.
    ProviderOwned,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Ordered conversation. Empty `messages` + non-empty `prompt` is also valid.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// Convenience single-turn prompt.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Optional system instruction.
    #[serde(default)]
    pub system: Option<String>,
    /// Provider-specific model id.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional max output tokens (best-effort; providers may ignore).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Tools the provider is allowed to call this turn. Empty = chat-only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    /// Endpoint a `ProviderOwned` provider can reach to bridge tool calls
    /// back into the executor. Set by the executor; ignored by
    /// `ExecutorOwned` providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_endpoint: Option<McpEndpoint>,
    /// In-process dispatcher handle for `ProviderOwned` providers that
    /// can't use the MCP loopback path — e.g. the codex app-server
    /// provider, which talks JSON-RPC to a subprocess and bridges
    /// approval requests + tool calls back through this dispatcher so
    /// the agentd permission engine fires. `None` for ExecutorOwned
    /// providers and for MCP-loopback providers (claude).
    #[serde(skip)]
    pub dispatcher: Option<Arc<dyn Dispatcher>>,
    /// Caller identity to attribute dispatched calls to. Pairs with
    /// `dispatcher`; the executor populates both together. The provider
    /// passes this back into `dispatcher.dispatch(caller, ...)` so the
    /// runner allowlist + interface allowlist layers of the permission
    /// engine resolve correctly.
    #[serde(skip)]
    pub caller: Option<Caller>,
}

impl std::fmt::Debug for CompletionRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionRequest")
            .field("messages", &self.messages)
            .field("prompt", &self.prompt)
            .field("system", &self.system)
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("tools", &self.tools)
            .field("mcp_endpoint", &self.mcp_endpoint)
            .field(
                "dispatcher",
                &self.dispatcher.as_ref().map(|_| "<dyn Dispatcher>"),
            )
            .field("caller", &self.caller)
            .finish()
    }
}

impl CompletionRequest {
    pub fn prompt(text: impl Into<String>) -> Self {
        Self {
            prompt: Some(text.into()),
            ..Default::default()
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Single string view: system + messages + prompt flattened. Providers that
    /// only accept a single prompt (CLI wrappers) call this.
    pub fn flatten(&self) -> String {
        let mut out = String::new();
        if let Some(sys) = &self.system {
            out.push_str(sys);
            out.push_str("\n\n");
        }
        for m in &self.messages {
            let tag = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            out.push_str(&format!("[{tag}] {}\n", m.content));
        }
        if let Some(p) = &self.prompt {
            out.push_str(p);
        }
        out
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub text: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// Tool calls the assistant wants the executor to run. Empty when the
    /// turn is a plain text reply (or when the provider already handled the
    /// loop internally — see [`LoopMode::ProviderOwned`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    // Messages must be self-explanatory: they render under a
    // `provider `<name>`: ` wrap, so no generic prefixes here (the final
    // render stays at two colons or fewer).
    #[error("{0}")]
    Config(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error("empty response")]
    EmptyResponse,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// Whether the provider returns `tool_calls` for the executor to dispatch
    /// (`ExecutorOwned`) or runs the agent loop on its own and bridges back
    /// via MCP (`ProviderOwned`). Defaults to executor-owned because that's
    /// what every API-style provider does.
    fn loop_mode(&self) -> LoopMode {
        LoopMode::ExecutorOwned
    }

    /// A fixed bearer token the executor should give the MCP loopback for this
    /// provider, instead of minting a fresh random one per call. Needed when a
    /// provider can only learn the token once (codex bakes it into a reused
    /// subprocess's environment at spawn). `None` → the executor mints a random
    /// per-invocation token, which suits header-based providers like claude.
    fn preferred_mcp_token(&self) -> Option<String> {
        None
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError>;
}

//! Unified LLM provider abstraction.
//!
//! TODO: every model surface (Claude CLI/API/SDK, Codex, Copilot, OpenAI, ...)
//! plugs in as a `Provider`.
//!
//! provider list:
//! - `MockProvider`      - deterministic, for tests
//! - `ClaudeCliProvider` - shells out to `claude -p`
//! - `CodexCliProvider`  - shells out to `codex exec --json`

pub mod providers;
pub mod registry;
pub mod types;

pub use providers::{
    ClaudeApiProvider, ClaudeCliProvider, CodexAppServerProvider, CodexCliProvider, MockProvider,
    OpenAiApiProvider, normalize_anthropic_endpoint, normalize_openai_endpoint,
};
pub use registry::ProviderRegistry;
pub use types::{
    CompletionRequest, CompletionResponse, LoopMode, McpEndpoint, Message, Provider, ProviderError,
    Role, ToolCall, ToolDef,
};

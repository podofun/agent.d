# agentd-ai

Model abstraction. One `Provider` trait, every vendor plugs in behind it.

Unified `CompletionRequest` / `CompletionResponse`. A request can carry an optional
`Arc<dyn Dispatcher>` + `Caller` so providers bridge tool calls + approvals back
through the executor without going via HTTP MCP.

`loop_mode()` splits providers into `ExecutorOwned` (executor drives the tool-use
loop) and `ProviderOwned` (CLI/MCP providers own their own loop).

Providers:

- `MockProvider` — tests.
- `ClaudeCliProvider` — shells `claude -p`, MCP loopback with `--allowedTools "mcp__agentd__*"` (ProviderOwned).
- `ClaudeApiProvider` — Anthropic Messages API (ExecutorOwned).
- `CodexAppServerProvider` — drives `codex app-server` over JSON-RPC, MCP-only, bridges approvals to the permission engine.
- `CodexCliProvider` — text-only fallback (`codex exec` has no allowlist flag).

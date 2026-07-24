# agentd-ai

`agentd-ai` defines the common interface for model providers and stores configured providers by name.

It provides:

- Request, response, message, tool-call, and provider error types.
- The `Provider` trait and the `ProviderRegistry`.
- Anthropic API, OpenAI-compatible API, Claude CLI, Codex CLI, Codex app-server, and mock providers.
- Executor-owned and provider-owned tool loops through `LoopMode`.

API providers return tool calls for the executor to run. CLI and app-server providers can own the loop and route tool calls or approval checks back through agentd.

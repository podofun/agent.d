# Providers

Providers are the backends agent.d uses to make model calls. This page explains how model selection works, which providers are registered, and what permissions are required.

## Model selection

Every model call in agent.d uses a string of the form `"<provider>/<model_id>"`. You pass this string in `agentd.runner({ model = … })` or in the `opts` to `ctx.ai.ask` / `ctx.ai.complete`.

```lua
agentd.runner({
  name = "reviewer",
  model = "anthropic/claude-opus-4-7",
})
```

If you omit the provider prefix, agent.d uses the default provider (`anthropic` unless `runtime.default_provider` says otherwise):

```lua
-- These two are equivalent:
ctx.ai.ask("Summarise this diff", { model = "claude-opus-4-7" })
ctx.ai.ask("Summarise this diff", { model = "anthropic/claude-opus-4-7" })
```

## Registered prefixes

Five built-in prefixes are registered at startup, plus one per `[providers.<name>]` entry in `config.toml` (see [Custom providers](/v0/providers/custom)):

| Prefix | Backend |
|---|---|
| `anthropic` | Anthropic Messages API (key from the secret store) |
| `anthropic-cli` | Local `claude` CLI |
| `openai` | OpenAI-compatible Messages API (key from the secret store) |
| `codex` | `codex app-server` over JSON-RPC |
| `openai-cli` | Local `codex` CLI text fallback |
| *your own* | Any OpenAI- or Anthropic-compatible endpoint declared in `config.toml` — OpenRouter, Groq, Together, vLLM, Ollama, LM Studio, gateways |

## Tool-use loop cap

When a runner drives an agentic loop (the model calls tools repeatedly), agent.d caps the number of turns at `runtime.max_turns`. The default is **16**. You can raise or lower it in `config.toml`:

```toml
[runtime]
max_turns = 32
```

## Listing providers at runtime

Call `ctx.ai.providers()` from any action or service to see which prefixes are available on the running daemon:

```lua
agentd.action("debug.providers", function(args, ctx)
  return ctx.ai.providers()
end)
```

## Permissions

Every model call requires the `ai:<provider>` grant for the provider being used. Grant it in `grants.toml` on the tool, runner, or service that makes the call:

```toml
[tool.mytools]
granted = ["ai:anthropic"]

[service.my_bot]
granted = ["ai:openai"]
```

Use `ai:*` to allow any provider (grant sparingly).

See [permission slugs](/v0/security/permission-slugs) and [grants](/v0/security/grants) for details.

## Provider pages

- [Anthropic](/v0/providers/anthropic) — `anthropic` and `anthropic-cli`
- [OpenAI](/v0/providers/openai) — `openai`
- [CLI backends](/v0/providers/cli-backends) — `anthropic-cli` and `openai-cli`
- [Codex](/v0/providers/codex) — `codex`
- [Custom providers](/v0/providers/custom) — OpenAI/Anthropic-compatible endpoints and local servers via `config.toml`
- [Credentials](/v0/providers/credentials) — storing API keys in the keyring
- [MCP loopback](/v0/providers/mcp) — exposing tools to the `claude` CLI

## See also

- [ctx.ai](/v0/reference/ctx/ai) — `ctx.ai.ask`, `ctx.ai.complete`, `ctx.ai.providers`
- [Runners](/v0/concepts/runners) — composing a model + skills + action allowlist
- [Security grants](/v0/security/grants) — granting `ai:<provider>` to components
- [Credentials](/v0/providers/credentials) — how provider keys are stored

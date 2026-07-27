# Providers

A provider sends a model request from agent.d to a model service or a local application.

This page explains provider names, model selection, turn limits, and permissions.

## Select a model

Use a model string with this form:

```text
<provider>/<model-id>
```

The text before the first `/` is the provider name, and the text after the first `/` is the model ID.

The following runner example selects the `anthropic` provider:

```lua
agentd.runner({
    name = "reviewer",
    model = "anthropic/claude-opus-4-7",
})
```

If the string has no `/`, agent.d uses `anthropic` as the initial default provider.

Use `runtime.default_provider` in `config.toml` to select a different default:

```toml
[runtime]
default_provider = "openai"
```

These calls are equivalent when `anthropic` is the default provider:

```lua
ctx.ai.ask("Summarize this diff.", {
    model = "claude-opus-4-7",
})

ctx.ai.ask("Summarize this diff.", {
    model = "anthropic/claude-opus-4-7",
})
```

Always include the provider name when a model ID contains `/` because agent.d keeps all `/` characters after the first `/`.

## Built-in providers

At startup, agent.d registers five built-in providers and each custom provider from `config.toml`.

| Provider | Purpose |
|---|---|
| `anthropic` | Calls the Anthropic Messages API. |
| `anthropic-cli` | Starts the local `claude` command for each call. |
| `openai` | Calls the OpenAI Chat Completions API. |
| `codex` | Starts and controls one local `codex app-server` process. |
| `openai-cli` | Starts `codex exec` for a text-only `ctx.ai` call. |

Use `ctx.ai.providers()` to get a sorted list of registered provider names without a permission.

```lua
agentd.action({
    name = "debug.providers",
    handler = function(_, ctx)
        return ctx.ai.providers()
    end,
})
```

## Set the turn limit

During a runner call, the `anthropic`, `openai`, and custom API providers can ask agent.d to call an action.
After agent.d calls a permitted action, it sends the result to the provider in a new turn.

The `runtime.max_turns` value limits this loop to 16 turns by default, and a value of `0` gives a one-turn limit.

```toml
[runtime]
max_turns = 32
```

The `runtime.max_turns` value does not limit `anthropic-cli` or `codex` because the local applications control their own loops.

## Give permissions

A `ctx.ai.ask` or `ctx.ai.complete` call requires the applicable `ai:<provider>` permission.
Give the permission to the tool or service that makes the call.

```toml
[tool.review]
granted = ["ai:anthropic"]

[service.triage_bot]
granted = ["ai:openai"]
```

The `ai:*` permission gives access to all registered providers, but use a provider-specific permission when possible.

## Provider pages

- [Anthropic](/v0/providers/anthropic) explains the `anthropic` provider.
- [OpenAI](/v0/providers/openai) explains the `openai` provider.
- [CLI providers](/v0/providers/cli-backends) explains `anthropic-cli` and `openai-cli`.
- [Codex](/v0/providers/codex) explains the `codex` provider.
- [Custom providers](/v0/providers/custom) explains API endpoints in `config.toml`.
- [Credentials](/v0/providers/credentials) explains keys in the OS keyring.

## See also

- [ctx.ai](/v0/reference/ctx/ai)
- [Runners](/v0/concepts/runners)
- [Security grants](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)

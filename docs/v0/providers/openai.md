# OpenAI provider

The `openai` prefix connects agent.d to an **OpenAI-compatible Messages API**. Use it for OpenAI models or any provider that speaks the same API surface.

## Credential setup

The provider reads your API key from the OS keyring (secret store). Store it once through a setup action:

```lua
agentd.action("setup.openai_key", function(args, ctx)
  ctx.secret.set("openai_api_key", args.key)
  return "stored"
end)
```

::: info
The exact secret key name the provider looks up is an internal implementation detail. See [Credentials](/v0/providers/credentials) for the recommended pattern for storing and managing API keys.
:::

::: warning Never hardcode API keys
Keep keys out of Lua files, `config.toml`, and version control. Use the keyring.
:::

## Using the provider

Reference `openai/<model_id>` in a runner or inline call:

```lua
agentd.runner({
  name = "summariser",
  model = "openai/gpt-5.5",
  skills = { "summarise" },
})
```

```lua
local reply = ctx.ai.ask("Summarise this PR description", {
  model = "openai/gpt-5.5",
})
```

## Required permission

Any component that calls this provider must hold the **`ai:openai`** grant:

```toml
# grants.toml
[tool.summary]
granted = ["ai:openai"]

[service.triage_bot]
granted = ["ai:openai"]
```

## See also

- [Providers overview](/v0/providers/) — model selection, max_turns, all prefixes
- [CLI backends](/v0/providers/cli-backends) — `openai-cli` for key-free local use via `codex`
- [Credentials](/v0/providers/credentials) — storing and rotating API keys
- [ctx.ai](/v0/reference/ctx/ai) — full API reference for model calls

# Anthropic provider

The `anthropic` prefix connects agent.d to the **Anthropic Messages API**. It is the default provider — you can omit the prefix and agent.d will route the call here automatically.

## Credential setup

The provider reads your API key from the OS keyring (secret store). Store it once with `agentctl secret set` — a running daemon picks it up immediately:

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

::: info
Storing credentials in the OS keyring keeps them out of your Lua files and config. See [Credentials](/v0/providers/credentials) for the full pattern, including managing keys programmatically via `ctx.secret`.
:::

::: warning Never hardcode API keys
Do not put API keys in `init.lua`, `config.toml`, or environment variables checked into version control. Use the keyring.
:::

## Using the provider

Reference `anthropic/<model_id>` in a runner definition or an inline `ctx.ai` call:

```lua
agentd.runner({
  name = "code_reviewer",
  model = "anthropic/claude-opus-4-7",
  skills = { "reviewer" },
  actions = { "git.diff", "git.status" },
})
```

```lua
-- inline one-shot call
local reply = ctx.ai.ask("What does this function do?", {
  model = "anthropic/claude-opus-4-7",
  system = "You are a concise code reviewer.",
})
```

Omitting the provider prefix also routes to `anthropic`:

```lua
ctx.ai.ask("Summarise the diff", { model = "claude-opus-4-7" })
```

## Required permission

Any tool, runner, or service that calls this provider must hold the **`ai:anthropic`** grant:

```toml
# grants.toml
[tool.review]
granted = ["ai:anthropic"]
```

## See also

- [Providers overview](/v0/providers/) — model selection, max_turns, all prefixes
- [CLI backends](/v0/providers/cli-backends) — `anthropic-cli` for key-free local use
- [Credentials](/v0/providers/credentials) — storing and rotating API keys
- [ctx.ai](/v0/reference/ctx/ai) — full API reference for model calls

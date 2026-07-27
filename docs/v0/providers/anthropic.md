# Anthropic Provider

The `anthropic` provider sends requests to the Anthropic Messages API.
agent.d sends each request to `https://api.anthropic.com/v1/messages`.

## Store the API key

The provider reads the API key from the `anthropic_api_key` secret when each model call starts.

Use this command to store the key:

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

The next model call uses the new key, so you do not have to restart agent.d.

::: warning Protect the API key
Do not put the API key in a Lua file or `config.toml`.
Do not commit the API key to a repository.
Use `agentctl secret` to put the key in the OS keyring.
:::

## Use the provider

To select this provider for a runner, use `anthropic/<model-id>`:

```lua
agentd.runner({
    name = "code_reviewer",
    model = "anthropic/claude-opus-4-7",
    skills = { "reviewer" },
    actions = { "git.diff", "git.status" },
})
```

The provider lets a runner call actions, and agent.d checks each action call in the action loop.

Use the same model string for a direct `ctx.ai` call, which does not give agent.d actions to the provider:

```lua
local reply = ctx.ai.ask("What does this function do?", {
    model = "anthropic/claude-opus-4-7",
    system = "Give a short code review.",
})
```

The agent.d default model for this provider is `claude-opus-4-7`.
Use `anthropic/` when you want the provider to use this default.

If `anthropic` is the default provider, you can omit `anthropic/`.

```lua
ctx.ai.ask("Summarize the diff.", {
    model = "claude-opus-4-7",
})
```

## Give permission to a `ctx.ai` caller

A `ctx.ai` call to this provider requires the `ai:anthropic` permission.
Give this permission to the tool or service that makes the call.

```toml
[tool.review]
granted = ["ai:anthropic"]
```

The caller does not need the `secret:anthropic_api_key` permission.
agent.d reads the API key from the OS keyring and does not give the key to the Lua code.

## See also

- [Providers](/v0/providers/)
- [Credentials](/v0/providers/credentials)
- [CLI providers](/v0/providers/cli-backends)
- [ctx.ai](/v0/reference/ctx/ai)

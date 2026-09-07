# CLI Providers

Each CLI provider starts a command from a local terminal application and uses the authentication data of that application.
CLI providers do not use API keys from the agent.d keyring.

| Provider | Command | Supported calls |
|---|---|---|
| `anthropic-cli` | `claude` | `ctx.ai` calls and runner calls |
| `openai-cli` | `codex exec` | Text-only `ctx.ai` calls |

## Prepare the terminal applications

Install and authenticate Claude Code before you use `anthropic-cli`.

Install and authenticate Codex before you use `openai-cli`.

Put the applicable command on the agent.d process `PATH` because the provider call fails when agent.d cannot find the command.

On Windows, agent.d can use native programs and these launcher files:

- `.ps1`
- `.cmd`
- `.bat`

## Use `anthropic-cli` with `ctx.ai`

A `ctx.ai` call starts `claude -p` and returns the text from the command.
Because a direct `ctx.ai` call does not supply an action list, this call cannot use agent.d actions.

```lua
local reply = ctx.ai.ask("Summarize this text.", {
    model = "anthropic-cli/sonnet",
})
```

The call requires the `ai:anthropic-cli` permission:

```toml
[tool.summary]
granted = ["ai:anthropic-cli"]
```

## Use `anthropic-cli` with a runner

During a runner call, agent.d gives the action list to `claude`, which calls the actions through a private local connection to agent.d.

```lua
agentd.runner({
    name = "local_reviewer",
    model = "anthropic-cli/sonnet",
    actions = { "git.diff" },
})
```

Add each permitted action to the runner entry in `grants.toml`:

```toml
[runner.local_reviewer]
granted = ["ai:anthropic-cli"]
allowed_actions = ["git.diff"]
```

Each action also needs its tool permissions because the `actions` field in Lua does not give permission to call an action.

## Use `openai-cli` with `ctx.ai`

For each `openai-cli` call, agent.d starts `codex exec` with the `read-only` sandbox and the `never` approval policy.
This provider returns text only and cannot call agent.d actions.

```lua
agentd.tool({
    name = "assist",
    requires = { "ai:openai-cli" },
})

agentd.action({
    name = "assist.ask",
    requires = { "ai:openai-cli" },
    handler = function(args, ctx)
        return ctx.ai.ask(args.prompt, {
            model = "openai-cli/",
        })
    end,
})
```

The call requires the `ai:openai-cli` permission:

```toml
[tool.assist]
granted = ["ai:openai-cli"]
```

::: warning Do not use `openai-cli` in a runner
A runner gives its action list to the selected provider, but `openai-cli` cannot use this list and fails before Codex starts.
Use the `codex` provider when a runner must call agent.d actions.
:::

## See also

- [Providers](/v0/providers/)
- [Codex provider](/v0/providers/codex)
- [Anthropic provider](/v0/providers/anthropic)
- [Credentials](/v0/providers/credentials)

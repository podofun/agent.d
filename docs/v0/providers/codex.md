# Codex Provider

The `codex` provider starts and controls a local Codex app server that supports agent.d actions during a runner call.

Unlike `codex`, the `openai-cli` provider starts `codex exec` for a text-only `ctx.ai` call.

## Prepare Codex

Install and authenticate Codex, and make sure that the `codex` command is on the `PATH` of the agent.d process.

Do not start the app server separately because agent.d cannot attach to a server that it did not start.

## Understand the app-server process

The first provider call starts this command:

```text
codex app-server --listen stdio://
```

agent.d communicates with the app-server process through standard input and standard output, and it uses the same process for later calls.
During normal cleanup, agent.d stops this app-server process.

If the app server exits unexpectedly, the provider does not start another process, and you must restart agent.d.

## Understand each call

For each call, the provider starts a temporary Codex thread that Codex does not save to a rollout file.
While a call is active, the provider waits before it starts another call.

agent.d sets the Codex sandbox to `read-only`, the approval policy to `untrusted`, and the reasoning effort to `low`.

agent.d disables the built-in Codex tools, so Codex can use only the runner actions through a private local connection to agent.d.

The Codex provider has a 180-second turn timeout. Runner calls also have a whole-run deadline, which defaults to 120 seconds; see [runner request limits](/v0/reference/protocol#runners-run).

## Use the provider with a runner

Use `codex/<model-id>` to select a model, or use `codex/` to let Codex select its default model.

```lua
agentd.runner({
    name = "codex_reviewer",
    model = "codex/",
    actions = { "git.diff", "git.status" },
})
```

Add each permitted action to the runner entry in `grants.toml`:

```toml
[runner.codex_reviewer]
granted = ["ai:codex"]
allowed_actions = ["git.diff", "git.status"]
```

Each action also needs its tool permissions because the `actions` field in Lua does not give permission to call an action.

## Use the provider with `ctx.ai`

A direct `ctx.ai` call gives Codex no agent.d actions, and the built-in Codex tools remain disabled.
Use this form only when you need a text response.

```lua
local reply = ctx.ai.ask("Explain this code.", {
    model = "codex/",
})
```

The call requires the `ai:codex` permission:

```toml
[tool.codex_assist]
granted = ["ai:codex"]
```

## See also

- [Providers](/v0/providers/)
- [CLI providers](/v0/providers/cli-backends)
- [ctx.ai](/v0/reference/ctx/ai)
- [Security grants](/v0/security/grants)

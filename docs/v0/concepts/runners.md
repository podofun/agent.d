# Runners

A runner is a named AI agent. It pairs a language model with a set of skills and a list of actions it can call. When you send a runner a prompt, it talks to the model, runs the actions the model asks for, and returns the final answer.

## Defining a runner

You define a runner in Lua:

```lua
agentd.runner({
  name    = "backend_reviewer",
  model   = "anthropic/claude-opus-4-7",
  system  = "Reply in plain text. No markdown headers.",
  skills  = { "reviewer" },
  actions = { "git.diff", "git.status" },
})
```

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Unique identifier for this runner. |
| `model` | yes | `"<provider>/<model_id>"` string. Omit the prefix to default to `anthropic`. |
| `system` | no | System prompt text. It comes after the text of the runner's skills. |
| `skills` | no | Ordered list of skill names to include in the system prompt. |
| `actions` | no | The actions the model can see and call. See [The action allowlist](#the-action-allowlist). |

## How a prompt flows

When a caller starts a run with `runners.run` (or `ctx.run`), agent.d:

1. Builds the system prompt from the runner's skills and its `system` text.
2. Checks the runner’s `ai:<provider>` grant and `[denial]`, then sends the prompt and history to the named provider.
3. If the model returns a tool call, checks that the action is one of the runner's actions, then checks its permissions. If the call is allowed, it runs the action.
4. Sends the result back to the model, and the loop continues.
5. The loop ends when the model returns a final text response. A run outside a session also ends when it reaches `max_turns`.

`runtime.max_turns` (default `16`) limits the tool-call loop for runs outside a session, so a one-off call stops after that many rounds. A run in a [session](/v0/reference/ctx/sessions) is a conversation someone can cancel, so it has no turn limit and keeps going until the model is done. You can change the limit in `config.toml`:

```toml
[runtime]
max_turns = 32
```

## The action allowlist

A runner has two action lists, each with its own job:

- The `actions` field in Lua lists the actions that the model can see and call. These are the only actions the model can call, whatever `grants.toml` allows.
- `allowed_actions` in `[runner.<name>]` of `grants.toml` lists the actions that the runner may call without a question. If the model calls one of its actions that is not on this list, agent.d asks first.

The action's permissions come from its tool's grants. Neither list grants them.

```toml
[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]
approval = "human"
```

The `approval` key sets who answers the runner's questions. See [Who answers](/v0/security/approvals#who-answers).

::: info Relation to grants
See [Permissions](/v0/concepts/permissions) for how a call is decided and [grants reference](/v0/security/grants) for `grants.toml` syntax.
:::

## Invoking a runner

From the console:

```bash [release]
agentctl runner run backend_reviewer "Review the staged changes for correctness."
```

```bash [cargo]
cargo run -p agentd-cli -- runner run backend_reviewer "Review the staged changes."
```

From an action or service, use `ctx.run`:

```lua
local result = ctx.run("backend_reviewer", "Review the staged changes.")
-- result: { text, provider, model, stop_reason? }
```

You can also pass a table for more control:

```lua
local result = ctx.run("backend_reviewer", {
  prompt  = "Review the staged changes.",
  model   = "anthropic/claude-opus-4-7",   -- override the runner's default
})
```

## See also

- [Skills](/v0/concepts/skills)
- [Permissions](/v0/concepts/permissions)
- [Providers](/v0/providers/)
- [Writing runners](/v0/writing/runners)

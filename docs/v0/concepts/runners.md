# Runners

A runner is a named AI worker. It pairs a language model with a set of skills and an advisory action allowlist, then handles prompt-to-response loops on behalf of callers.

## Defining a runner

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
| `system` | no | System prompt text. Appended after the composed skill bodies (skills come first, the runner's `system` last). |
| `skills` | no | Ordered list of skill names to compose into the system prompt. |
| `actions` | no | Advisory action allowlist (see below). |

## How a prompt flows

When a caller invokes `runners.run` (or `ctx.run`), the runtime:

1. Composes the system prompt from the runner's `system` text and its resolved skills.
2. Sends the prompt to the model via the named provider.
3. If the model returns a tool call, the runtime checks the permission engine and, if approved, dispatches the action.
4. The result is fed back to the model and the loop continues.
5. The loop ends when the model returns a final text response or `max_turns` is reached.

`runtime.max_turns` (default `16`) caps the tool-use loop per runner call. You can override it in `config.toml`:

```toml
[runtime]
max_turns = 32
```

## The action allowlist

The `actions` field on a runner is an **advisory** allowlist — permission layer 3. It narrows which actions a runner may call, but it does not grant any capabilities by itself. `grants.toml` is authoritative:

```text
tool/package grants ∩ action.requires ∩ runner.allow ∩ interface.allow ∩ policy = Decision
```

A runner with no `actions` list has no constraint at layer 3 — the other four layers still apply. If you set `allowed_actions` in `grants.toml` for a runner, that overrides (or narrows) what the runner definition says.

```toml
[runner.backend_reviewer]
allowed_actions = ["git.diff", "git.status"]
```

::: info Relation to grants
See [Permissions](/v0/concepts/permissions) for the full five-layer model and [grants reference](/v0/security/grants) for `grants.toml` syntax.
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

# Writing Runners

A **runner** is a named AI agent: a model, an optional system prompt, a set of skills, and the actions it can call. You register runners in Lua and then call them from the CLI, from another action, or from a chat. This page covers `agentd.runner` and how each field shapes the runner's behaviour.

## Registration

Register a runner with `agentd.runner`:

```lua
agentd.runner({
  name    = "backend_reviewer",
  model   = "anthropic/claude-opus-4-7",
  system  = "Reply in plain text. No markdown headers.",
  skills  = { "reviewer" },
  actions = { "git.diff", "git.status" },
})
```

All fields except `name` and `model` are optional.

## Fields

| Field | Type | Description |
|---|---|---|
| `name` | `string` | Unique identifier. Used in `agentctl runner run <name>` and `ctx.run(name, …)`. |
| `model` | `string` | Which provider and model to use. See [Model selection](#model-selection). |
| `system` | `string?` | The runner's own instructions, added after its skills in the system prompt. |
| `skills` | `string[]?` | Names of registered skills to include in the system prompt. |
| `actions` | `string[]?` | The actions the model can see and call. See [The `actions` list](#the-actions-list). |
| `compact` | `table \| false?` | How a long chat session is summarized. See [Compaction](#compaction). Leave it out for the defaults. |

## Model selection

The `model` string has the form `"<provider>/<model_id>"`. The prefix chooses the provider that runs the request:

| Prefix | Backend |
|---|---|
| `anthropic` | Anthropic Messages API |
| `anthropic-cli` | Local `claude` CLI |
| `openai` | OpenAI-compatible API |
| `codex` | `codex app-server` over JSON-RPC |
| `openai-cli` | Local `codex` CLI text fallback |

If you omit the prefix, the daemon defaults to `anthropic`.

```lua
model = "anthropic/claude-opus-4-7"
model = "openai/gpt-5.5"
```

See [Providers](/v0/providers/) for setup and credential details.

## How skills and system compose

When a runner runs, the daemon builds its system prompt in this order:

1. The body text of each skill listed in `skills` (in list order).
2. The runner's own `system` string, if set.

Skills are reusable blocks of instructions, written as Markdown files or inline Lua tables (see [Writing skills](/v0/writing/skills)). By combining several small skills, you can share instructions between runners and write each one once.

```lua
agentd.runner({
  name   = "reviewer",
  model  = "anthropic/claude-opus-4-7",
  skills = { "reviewer", "terse" },   -- reviewer body first, then terse
  system = "Focus only on the staged diff.",
})
```

## The `actions` list

`actions` lists the tools that the model can see and call. The model can call only the actions in this list (plus any that its skills list), whatever `grants.toml` allows.

Permissions come from [grants.toml](/v0/security/grants). If the model calls one of its actions that is missing from the runner's `allowed_actions` there, agent.d asks before the action runs. The action's tool also needs every permission that the action requires.

```toml
# grants.toml
[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]
```

::: tip
Keep `actions` and `allowed_actions` the same, so the model sees exactly the actions it may call without a question.
:::

## Calling a runner

From Lua:

```lua
-- simple string prompt
local result = ctx.run("backend_reviewer", "Review the staged changes.")

-- structured options
local result = ctx.run("backend_reviewer", {
  prompt  = "Review the staged changes.",
  model   = "anthropic/claude-opus-4-7",  -- override per-call
  system  = "Extra instruction.",
})
-- result: { text, provider, model, stop_reason? }
```

To keep a conversation between calls, pass `session_id` and `prompt` to `ctx.run`. See the complete [session example](/v0/reference/ctx/sessions#running-a-session), including its tool registration and permission grants.

From the CLI:

```bash
agentctl runner run backend_reviewer "Review the staged changes."
```

Outside a session, `runtime.max_turns` (default 16) limits how many rounds of tool calls a run can make on providers that agent.d drives itself. A run in a session has no turn limit and keeps going until the model is done or the run is cancelled.

## Compaction

A long chat session can outgrow the model's context. Compaction keeps it manageable by replacing older messages with a summary.

Before a session run, the daemon checks whether at least 60 messages are stored. If so, it asks the model selected for that run to summarize the older messages and keeps at least the newest 20 unchanged. Each user message, assistant message and tool result counts as a turn. The daemon keeps more messages when it needs to keep a tool call together with its result, and it skips compaction if it finds no suitable place to cut.

Use `compact` to change these defaults or turn compaction off:

```lua
agentd.runner({
  name = "support",
  model = "anthropic/claude-sonnet-5",
  compact = {
    after_turns = 40,   -- compact once this many turns are stored (default 60)
    keep_recent = 10,   -- minimum recent messages to keep (default 20)
    prompt = "Summarize the conversation, keeping every order number.", -- optional
  },
})

agentd.runner({ name = "oneshot", model = "anthropic/claude-sonnet-5", compact = false })
```

The fields `after_turns`, `keep_recent` and `prompt` are optional, and `keep_recent` must be smaller than `after_turns`. If the summary call fails, the daemon logs a warning and continues with the full history. A successful compaction is saved before the runner answers, so it stays stored even if the run then fails.

## See also

- [Runners concept](/v0/concepts/runners)
- [Writing skills](/v0/writing/skills)
- [Providers](/v0/providers/)
- [grants.toml reference](/v0/security/grants)

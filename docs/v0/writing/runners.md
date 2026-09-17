# Writing Runners

A **runner** is a named AI worker: a model, an optional system prompt, a set of
composed skills, and an advisory action allowlist. This page covers
`agentd.runner` and how each field shapes the runner's behaviour.

## Registration

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
| `model` | `string` | Model selection string — see [Model selection](#model-selection). |
| `system` | `string?` | Additional text prepended to the composed system prompt. |
| `skills` | `string[]?` | Names of registered skills to compose into the system prompt. |
| `actions` | `string[]?` | Advisory allowlist of actions the runner may call. |
| `compact` | `table \| false?` | Session compaction policy — see [Compaction](#compaction). Leave it out for the defaults. |

## Model selection

The `model` string has the form `"<provider>/<model_id>"`. The prefix routes to
a provider registered in the daemon:

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

When a runner is invoked, the daemon assembles the effective system prompt in
this order:

1. The body text of each skill listed in `skills` (in list order).
2. The runner's own `system` string, if set.

Skills are reusable instruction blocks — authored as Markdown files or inline
tables (see [Writing skills](/v0/writing/skills)). Composing multiple skills lets
you mix focused instruction sets without duplicating text.

```lua
agentd.runner({
  name   = "reviewer",
  model  = "anthropic/claude-opus-4-7",
  skills = { "reviewer", "terse" },   -- reviewer body first, then terse
  system = "Focus only on the staged diff.",
})
```

## The `actions` advisory allowlist

`actions` is a **hint** to the model about which tools it should consider calling.
It is **not** a security boundary — that role belongs to
[grants.toml](/v0/security/grants). A runner call adds a runner-layer allowlist to
the permission engine's five-layer intersection:

```
tool/package grants ∩ action.requires ∩ runner.allow ∩ interface.allow ∩ policy
```

So even if an action appears in `actions`, it will be denied unless the runner is
granted that action in `grants.toml`. Conversely, omitting an action from
`actions` does not block it if all permission layers are satisfied — `actions` is
advisory.

```toml
# grants.toml — actual enforcement
[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]
```

::: tip
Keep `actions` and `allowed_actions` in sync. The Lua field informs the model;
the `grants.toml` entry enforces policy.
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

`runtime.max_turns` (default 16) caps the tool-use loop per runner call for
executor-owned providers.

## Compaction

Before a session run, the daemon checks whether at least 60 messages are stored. If so, it asks the model selected for that run to summarize the older messages and keeps at least the newest 20 unchanged. Each user message, assistant message and tool result counts as a turn. The daemon keeps more messages when needed to preserve a complete tool exchange, or skips compaction if there is no suitable boundary.

Use `compact` to change these defaults or disable compaction:

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

The fields `after_turns`, `keep_recent` and `prompt` are optional; `keep_recent` must be smaller than `after_turns`. If the summary call fails, the daemon logs a warning and continues with the full history. A successful compaction is saved before the runner answers, so it remains stored even if the subsequent run fails.

## See also

- [Runners concept](/v0/concepts/runners)
- [Writing skills](/v0/writing/skills)
- [Providers](/v0/providers/)
- [grants.toml reference](/v0/security/grants)

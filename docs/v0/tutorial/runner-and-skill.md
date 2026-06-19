# Step 4 — Runner and Skill

A **skill** shapes what the model knows about its role; a **runner** wires a model, its skills, and an advisory action list into a named AI worker. This page adds both to the project.

## Write the skill

Create `skills/reviewer.md`. Skills are Markdown files with a YAML frontmatter block:

```markdown
---
name: reviewer
description: meticulous code reviewer
actions:
  - git.diff
  - git.status
---
You are a meticulous code reviewer. Focus on correctness, clarity, and test
coverage. Cite file paths and line numbers. Prefer concrete suggestions over
hand-waving.
```

The frontmatter fields:

| Field | Required | Meaning |
|-------|----------|---------|
| `name` | yes | The skill's identifier; referenced by runners. |
| `description` | no | Human-readable summary; shown by `agentctl skills ls`. |
| `actions` | no | Advisory list of actions the skill expects to use. |

Everything after the `---` closing fence is the system prompt fragment that gets composed into the runner's system prompt.

## Load skills in `init.lua`

You already added this line in [Step 1](/v0/tutorial/project-layout):

```lua
agentd.skills.dir("skills")
```

`agentd.skills.dir(path)` scans the directory relative to `init.lua` and loads every `*.md` file it finds. To load a single file instead, use `agentd.skills.load("skills/reviewer.md")`.

## Write the runner

Create `runners/backend_reviewer.lua`:

```lua
agentd.runner({
    name = "backend_reviewer",
    system = "Reply in plain text. No markdown headers.",
    model = "anthropic/claude-opus-4-7",
    skills = { "reviewer" },
    actions = { "git.diff", "git.status" },
})
```

### Fields explained

- **`name`** — the runner's identifier; used in `agentctl runner run <name>`.
- **`model`** — the model selection string in `"<provider>/<model_id>"` format. The prefix before `/` routes to a registered provider. `anthropic` routes to the Anthropic Messages API.
- **`system`** — an inline system prompt fragment added on top of composed skill prompts.
- **`skills`** — skill names to compose into the system prompt.
- **`actions`** — an advisory action allowlist. This is layer 3 of the permission engine; the grants file is still authoritative. An empty list means no constraint at this layer.

### Model selection string

The general form is `"<provider>/<model_id>"`. Registered provider prefixes:

| Prefix | Backend |
|--------|---------|
| `anthropic` | Anthropic Messages API |
| `anthropic-cli` | Local `claude` CLI |
| `openai` | OpenAI-compatible API |
| `codex` | `codex app-server` JSON-RPC |
| `openai-cli` | Local `codex` CLI |

## Grant the AI permission

Running a runner calls the model, which requires an `ai:anthropic` grant. Add it to `grants.toml`:

```toml
[tool.git]
granted = ["shell.exec:git"]

[runner.backend_reviewer]
allowed_actions = ["git.diff", "git.status"]
granted = ["ai:anthropic"]
```

::: warning Provider credentials required
The `ai:anthropic` grant tells the engine you allow the call, but the daemon also needs a real API key. Store your Anthropic key in the secret store before running the daemon. See [Provider credentials](/v0/providers/credentials) for the setup command.
:::

## Your project so far

You now have all five files. Here is the complete state before you start the daemon:

```
git-reviewer/
├── init.lua
├── grants.toml
├── tools/git.lua
├── skills/reviewer.md
└── runners/backend_reviewer.lua
```

## Next step

[Step 5 — Calling the agent →](/v0/tutorial/calling)

## See also

- [Concepts: runners](/v0/concepts/runners)
- [Concepts: skills](/v0/concepts/skills)
- [Writing runners](/v0/writing/runners)
- [Writing skills](/v0/writing/skills)
- [Provider credentials](/v0/providers/credentials)

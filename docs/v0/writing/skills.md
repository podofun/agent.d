# Writing Skills

A **skill** is a reusable instruction block that can be composed into one or
more runner system prompts. This page covers both ways to define a skill: a
Markdown file with YAML frontmatter, and an inline Lua call.

## Form 1 — Markdown file with YAML frontmatter

Create a `.md` file anywhere under your project tree. The YAML frontmatter
declares metadata; the body is the instruction text.

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

### Frontmatter fields

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes | Unique skill name, referenced by runners and `agentd.skills.load`. |
| `description` | `string` | no | Short description; returned by `skills.list` and `agentctl skills ls`. |
| `actions` | `string[]` | no | Advisory list of actions this skill expects its runner to call. |

### Loading skill files

Load a directory of `*.md` files at once:

```lua
-- loads every *.md under the skills/ directory
agentd.skills.dir("skills")    -- returns count loaded
```

Load a single file by path:

```lua
local name = agentd.skills.load("skills/reviewer.md")
```

Both calls resolve the path relative to `init.lua` and reject absolute paths and
`..` traversal — the same rules as `import`.

List all currently registered skill names:

```lua
local names = agentd.skills.list()   -- string[]
```

## Form 2 — Inline skill in Lua

When a skill is short or generated programmatically, define it directly:

```lua
agentd.skill({
  name        = "terse",
  description = "no preamble, no markdown headers",
  system      = "Reply in plain text. No preamble. No markdown headers.",
  actions     = {},          -- optional advisory action list
})
```

### Fields

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | `string` | yes | Unique skill name. |
| `description` | `string?` | no | Short description. |
| `system` | `string` | yes | The instruction text composed into the runner prompt. |
| `actions` | `string[]?` | no | Advisory action list. |

::: info
For Markdown skills the instruction text is the file body; for inline skills it
is the `system` field. The two forms are otherwise equivalent from a runner's
perspective.
:::

## Composing skills into a runner

Reference skills by name in `agentd.runner`:

```lua
agentd.runner({
  name   = "backend_reviewer",
  model  = "anthropic/claude-opus-4-7",
  skills = { "reviewer", "terse" },
  system = "Focus only on the staged diff.",
})
```

The daemon prepends each skill's text to the runner's system prompt in list
order, then appends the runner's own `system` string. Skills must be registered
before the runner that references them — load skill files before importing runner
files in `init.lua`.

## See also

- [Skills concept](/v0/concepts/skills)
- [Writing runners](/v0/writing/runners)
- [Writing init.lua](/v0/writing/init)
- [agentctl skills](/v0/reference/cli)

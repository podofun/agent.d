# Writing Skills

A **skill** is a block of instructions you write once and reuse in the system prompt of one or more runners. For example, a "reviewer" skill can describe how to review code, and every runner that reviews code can include it.

You can define a skill in two ways: as a Markdown file with YAML frontmatter, or inline with a Lua call. Runners treat both the same.

## Form 1: Markdown file with YAML frontmatter

Markdown files are the best fit for longer instructions. Create a `.md` file anywhere in your configuration directory. The YAML frontmatter holds the skill's details, and the body holds the instruction text.

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
| `name` | `string` | yes | Unique skill name, used by runners and `agentd.skills.load`. |
| `description` | `string` | no | Short description, shown by `skills.list` and `agentctl skills ls`. |
| `actions` | `string[]` | no | Actions this skill expects to use. A runner that includes the skill can see these actions. Permissions still come from `grants.toml`. |

### Loading skill files

To load every `*.md` file in a directory:

```lua
-- loads every *.md under the skills/ directory
local loaded = agentd.skills.dir("skills")    -- the skills it loaded
```

To load a single file:

```lua
local reviewer = agentd.skills.load("skills/reviewer.md")
print(reviewer.name, reviewer.description)
```

Both calls return live skills that you can read and change, as described in [Changing what you registered](/v0/writing/init#changing-what-you-registered).

Both calls resolve the path relative to `init.lua`, with the same rules as `import`: absolute paths and `..` are errors.

To list the names of every registered skill:

```lua
local names = agentd.skills.list()   -- string[]
```

## Form 2: Inline skill in Lua

For a short skill, or one your code builds, define it directly in Lua:

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
| `system` | `string` | yes | The instruction text added to the runner's system prompt. |
| `actions` | `string[]?` | no | Actions this skill expects to use, as in the Markdown form. |

::: info
In a Markdown skill the instruction text is the file body. In an inline skill it is the `system` field.
:::

## Composing skills into a runner

List skills by name in `agentd.runner`:

```lua
agentd.runner({
  name   = "backend_reviewer",
  model  = "anthropic/claude-opus-4-7",
  skills = { "reviewer", "terse" },
  system = "Focus only on the staged diff.",
})
```

The daemon builds the runner's system prompt from each skill's text in list order, followed by the runner's own `system` string. Every skill a runner lists must be registered. If one is missing, the run fails with an `unknown_skill` error.

## See also

- [Skills concept](/v0/concepts/skills)
- [Writing runners](/v0/writing/runners)
- [Writing init.lua](/v0/writing/init)
- [agentctl skills](/v0/reference/cli)

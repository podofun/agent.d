# Skills

A skill is a reusable block of instructions that shapes how a runner behaves. A runner's skills are added to its system prompt, so you can write shared instructions once and use them in many runners.

## Defining a skill

### Markdown file (recommended)

Create a `.md` file with YAML frontmatter:

```md
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

| Field | Meaning |
|---|---|
| `name` | Unique skill identifier. |
| `description` | Optional short summary, shown by `agentctl skills inspect`. |
| `actions` | Optional list of actions this skill uses. See [The `actions` field](#the-actions-field). |

Load skills from `init.lua`:

```lua
-- Load a single skill file
agentd.skills.load("skills/reviewer.md")

-- Load every *.md file under a directory
agentd.skills.dir("skills")
```

### Inline skill

When the skill is short enough to live in your Lua code:

```lua
agentd.skill({
  name        = "terse",
  description = "no preamble, no markdown headers",
  system      = "Reply in plain text. No preamble. No markdown headers.",
})
```

## Composing skills into a runner

List skills by name in the runner's `skills` array. They are added to the system prompt in the order listed:

```lua
agentd.runner({
  name   = "backend_reviewer",
  model  = "anthropic/claude-opus-4-7",
  system = "Reply in plain text.",   -- appended last
  skills = { "reviewer", "terse" },  -- composed first, in order
})
```

The skill texts come first, in the order listed, and the runner's own `system` text comes last.

## The `actions` field

The `actions` list names the actions a skill is meant to use. A runner that includes the skill can see and call these actions, in addition to its own `actions`. The list grants no permissions: those decisions happen in `grants.toml` and the runner's `allowed_actions`. `agentctl skills inspect` shows the list.

## Listing skills

```bash [release]
agentctl skills ls
agentctl skills inspect reviewer
```

```bash [cargo]
cargo run -p agentd-cli -- skills ls
```

## See also

- [Writing skills](/v0/writing/skills)
- [Runners](/v0/concepts/runners)
- [Tutorial: runner and skill](/v0/tutorial/runner-and-skill)

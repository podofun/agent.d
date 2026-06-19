# Skills

A skill is a reusable instruction fragment that shapes how a runner behaves. Skills are composed into the runner's system prompt at startup, so the model always has the right context without you repeating it in every runner definition.

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
| `description` | Optional human-readable summary; surfaced by `agentctl skills inspect`. |
| `actions` | Optional list of action names relevant to this skill (informational, not a grant). |

Load skills from `init.lua`:

```lua
-- Load a single skill file
agentd.skills.load("skills/reviewer.md")

-- Load every *.md file under a directory
agentd.skills.dir("skills")
```

### Inline skill

When you don't need a separate file:

```lua
agentd.skill({
  name        = "terse",
  description = "no preamble, no markdown headers",
  system      = "Reply in plain text. No preamble. No markdown headers.",
})
```

## Composing skills into a runner

List skills by name in the runner's `skills` array. They are appended to the system prompt in the order listed:

```lua
agentd.runner({
  name   = "backend_reviewer",
  model  = "anthropic/claude-opus-4-7",
  system = "Reply in plain text.",   -- appended last
  skills = { "reviewer", "terse" },  -- composed first, in order
})
```

Skill bodies are composed first, in the order listed; the runner's own `system` text is appended last.

## The `actions` field

The `actions` list in a skill's frontmatter is informational metadata — it does not constrain or grant anything. It surfaces in `agentctl skills inspect` so you can see which actions a skill is intended to be used with. The actual permission decisions happen in `grants.toml` and the runner's `allowed_actions`.

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

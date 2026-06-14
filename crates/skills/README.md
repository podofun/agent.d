# agentd-skills

Skill — a reusable behavior mode (reviewer, debugger, support).

- `SkillDef { name, description, system, actions }` — body is a system-prompt fragment; `actions` is an advisory allowlist.
- `SkillRegistry` — loads `*.md` files with YAML-ish frontmatter from a skills dir.

Skills are authored as Markdown (frontmatter + body) or inline via `agentd.skill{...}`.

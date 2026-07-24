# agentd-skills

`agentd-skills` parses and stores reusable runner behavior.

A skill contains a name, optional description, system-prompt text, and an advisory action list. `SkillRegistry` loads Markdown files recursively and also accepts definitions registered by Lua.

Skill files use a small frontmatter format with `name`, `description`, and `actions` keys. The Markdown body becomes the system-prompt text. An action listed by a skill is not a permission grant.

# Step 4: Add a Runner and a Skill

A skill gives task instructions to a model. A runner connects a model to the skill and action.

## Write the skill

Create `~/.config/agentd/skills/reviewer.md` with this content:

```markdown
---
name: reviewer
description: Review a staged Git diff.
actions:
  - git.diff
---
Use `git.diff` to get the staged diff. Review only that diff. Find defects and missing tests. Give a specific correction for each defect.
```

The frontmatter gives the skill name, description, and applicable action. The text after the frontmatter gives instructions to the model.

The `agentd.skills.dir("skills")` line in `init.lua` loads this file.

## Write the runner

Create `~/.config/agentd/runners/backend_reviewer.lua` with this content:

```lua
agentd.runner({
    name = "backend_reviewer",
    model = "anthropic-cli/sonnet",
    skills = { "reviewer" },
    actions = { "git.diff" },
})
```

The `name` field identifies the runner. You will use this name with `agentctl runner run`.

The `model` field selects a provider and model. The string has the form `provider/model`.

The `skills` field adds the review instructions to the system prompt. The `actions` field tells the model about `git.diff`.

The `actions` field is not a permission grant. The `allowed_actions` field in `grants.toml` is the permission limit.

## Select a provider

This tutorial uses `anthropic-cli`. It runs the `claude` command from the Claude Code terminal application.

You can use `anthropic-cli` when Claude Code is installed and authenticated. You can also use it when you do not want an Anthropic API key in agent.d.

To use the Anthropic API, set the model to `anthropic/<model-id>`. Replace `<model-id>` with an available Anthropic model ID.

Then, change the runner grant to `ai:anthropic`.

The `anthropic` prefix means the Anthropic API. This option requires an Anthropic API key in the agent.d secret store.

agent.d also supplies `openai-cli` for text-only model calls. This prefix runs the `codex` command from the Codex terminal application.

You can use `openai-cli` when Codex is installed and authenticated. You can also use it when you do not want an OpenAI API key in agent.d.

Currently, `openai-cli` cannot call agent.d actions. Thus, do not use it for this action-using Git reviewer.

For a text-only `ctx.ai` call, select `openai-cli/` and grant `ai:openai-cli`. Refer to [CLI providers](/v0/providers/cli-backends) for details.

Neither CLI provider needs an API key in the agent.d secret store. Each provider uses the authentication of its terminal application.

The terminal application also applies its own access and permission settings. The agent.d grants do not replace those settings.

::: warning The terminal application must be available
The `claude` or `codex` command must be on `PATH` for the `agentd` process. The provider call fails if its command is absent.
:::

## Configuration directory contents

Your configuration directory now contains these files:

```text
agentd/
├── init.lua
├── grants.toml
├── tools/
│   └── git.lua
├── skills/
│   └── reviewer.md
└── runners/
    └── backend_reviewer.lua
```

## Next step

[Step 5: Run the agent](/v0/tutorial/calling)

## See also

- [Runner concepts](/v0/concepts/runners)
- [Skill concepts](/v0/concepts/skills)
- [CLI providers](/v0/providers/cli-backends)
- [Provider credentials](/v0/providers/credentials)

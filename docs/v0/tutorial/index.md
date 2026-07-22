# Build Your First Agent

This tutorial shows you how to make a small Git review agent. The agent reads a staged diff and gives review comments.

You will put all agent.d files in one configuration directory. You do not need a separate directory for each agent.

## What you will make

The configuration will contain these items:

- One action runs `git diff --staged`.
- One skill gives review instructions to the model.
- One runner connects the model to the action and the skill.
- One grants file gives only the necessary permissions.

## Before you start

Install agent.d. Make sure that `agentd` and `agentctl` are on `PATH`. Refer to [Installation](/v0/guide/installation).

Make sure that `git` is on `PATH`.

You must also have one of these provider options:

| Option | Requirement |
|---|---|
| `anthropic` | An Anthropic API key in the agent.d secret store. |
| `anthropic-cli` | The Claude Code terminal application installed and authenticated. |

This tutorial uses `anthropic-cli`. This option uses your Claude Code authentication and does not require an Anthropic API key in agent.d.

## Tutorial steps

| Step | Page | Task |
|---|---|---|
| 1 | [Configuration directory](/v0/tutorial/config-directory) | Make the configuration directory and write `init.lua`. |
| 2 | [Git action](/v0/tutorial/first-tool) | Write an action that reads a staged Git diff. |
| 3 | [Permissions](/v0/tutorial/permissions) | Give the minimum permissions. |
| 4 | [Runner and skill](/v0/tutorial/runner-and-skill) | Add review instructions and a model runner. |
| 5 | [Run the agent](/v0/tutorial/calling) | Start agent.d and review a staged diff. |

Each step uses the same configuration directory.

## See also

- [What is agent.d?](/v0/guide/what-is-agentd)
- [How it works](/v0/guide/how-it-works)
- [Concepts overview](/v0/concepts/)

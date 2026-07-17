# Build Your First Agent

This tutorial walks you through creating a working git review agent from scratch. By the end you will have a running daemon that lets any client ask an AI to inspect staged changes in a repository.

## What you will build

A self-contained project that registers a `git` tool with two actions (`git.diff`, `git.status`), a Markdown skill that shapes the reviewer's persona, and a runner that wires them together under `anthropic/claude-opus-4-7`. You will call it interactively with `agentctl` and watch hot reload in action.

## Prerequisites

- agent.d installed and `agentd` + `agentctl` on your `PATH` — see [Installation](/v0/guide/installation).
- An Anthropic API key stored in the secret store (covered in [Step 5](/v0/tutorial/runner-and-skill)).
- `git` available on `PATH`.

## The six steps

| Step | Page | What you do |
|------|------|-------------|
| 1 | [Project layout](/v0/tutorial/project-layout) | Create the project directory and write a minimal `init.lua`. |
| 2 | [First tool](/v0/tutorial/first-tool) | Write `tools/git.lua` with two actions backed by `ctx.shell`. |
| 3 | [Permissions](/v0/tutorial/permissions) | Write `grants.toml` and understand why the daemon is default-deny. |
| 4 | [Runner & skill](/v0/tutorial/runner-and-skill) | Add a Markdown skill and a runner that uses the model. |
| 5 | [Calling the agent](/v0/tutorial/calling) | Start the daemon and exercise every piece with `agentctl`. |
| 6 | [Dev loop](/v0/tutorial/dev-loop) | Use `--watch`, `agentctl types`, and `agentctl trace` to iterate fast. |

## See also

- [What is agent.d?](/v0/guide/what-is-agentd)
- [How it works](/v0/guide/how-it-works)
- [Quick start](/v0/guide/quick-start)
- [Concepts overview](/v0/concepts/)

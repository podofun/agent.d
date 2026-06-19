# What is agent.d?

agent.d is a portable runtime for tool-using AI agents. It gives you one place to define what an agent can do — its tools, model, memory, external access, and approval rules — and then makes all of that available to any connected client under a default-deny permission engine.

## The problem it solves

Most agent prototypes start simple: a prompt, a model, and a few tool calls. They get harder to operate once you need to answer questions like:

- Which commands is this agent allowed to run?
- Can this chat bot reuse the same tools as the CLI?
- How do I stop one integration from getting access intended for another?
- Where should operational state live?
- How do I swap model providers without rewriting every tool?

These questions don't have clean answers when tool definitions, permission logic, and provider wiring live inside each frontend separately. Every new surface means duplicating and re-auditing the same code.

agent.d turns those concerns into runtime configuration instead of application glue. You write Lua to register components once. The daemon loads them, enforces policy, and serves any number of connected clients.

## Who it's for

**Agent builders** writing tools, runners, and services in Lua who want a clean separation between capability definitions and the clients that call them.

**Operators** who need to control what an agent can do at runtime — granting permissions, approving requests interactively, and observing activity through structured traces — without touching application code.

## How it helps

- **Define tools once.** Expose actions such as `git.status` or `deploy.preview`, then call them from any connected client.
- **Control access centrally.** `grants.toml` is the only source of grants. A component manifest can declare what it needs, but it can never grant itself access.
- **Fail closed by default.** A tool cannot touch the local system, network, secrets, or models unless it has an explicit grant.
- **Ask for approval when needed.** A privileged operator connected to the `/control` plane can approve a missing grant once or persist it for future runs.
- **Keep agent behavior portable.** Frontends reuse the same component definitions instead of carrying their own copies.
- **Keep operations visible.** The runtime writes structured trace events so tool calls and runner activity can be inspected later.
- **Use different providers.** Built-in providers cover the Anthropic API, OpenAI-compatible APIs, local CLI backends, and Codex app-server — selected per runner with a `"<provider>/<model_id>"` string.

## What you get

Two binaries:

- **`daemon`** — the runtime server. Loads your Lua components, enforces grants, and listens on `127.0.0.1:7777` by default.
- **`agentctl`** — the console client. Lets you call actions, inspect runners, follow traces, and manage packages from the terminal.

You write components in Lua and configure permissions in TOML. The daemon does the rest.

## Next steps

- [Installation](/v0/guide/installation) — build from source and put the binaries on your PATH.
- [Quick start](/v0/guide/quick-start) — start the daemon with the bundled example and make your first call in five minutes.

## See also

- [How it works](/v0/guide/how-it-works)
- [Concepts](/v0/concepts/)
- [Permissions](/v0/concepts/permissions)
- [Providers](/v0/providers/)

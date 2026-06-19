# Core Concepts

agent.d is built from a small set of named building blocks. Understanding each one — and how they fit together — makes the rest of the docs easier to follow.

## The building blocks

| Concept | What it is |
|---|---|
| [Runtime](/v0/concepts/runtime) | The daemon process that loads all components at startup and enforces policy on every call. |
| [Tools and Actions](/v0/concepts/tools-and-actions) | A tool is a named namespace; an action is one callable operation inside it, identified as `tool.action`. |
| [Runners](/v0/concepts/runners) | Named AI workers: a model, a set of skills, and an advisory action allowlist that handles prompt-to-response loops. |
| [Skills](/v0/concepts/skills) | Reusable instruction fragments composed into a runner's system prompt at startup. |
| [Services](/v0/concepts/services) | Long-running background Lua tasks (gateways, pollers) with restart supervision. |
| [Memory and State](/v0/concepts/memory-and-state) | Durable namespaced key/value storage (`ctx.memory`) that survives restarts, versus ephemeral per-reload state (`ctx.state`). |
| [Interfaces and Callers](/v0/concepts/interfaces-and-callers) | Interfaces are client surfaces (WebSocket today); every call carries a Caller identity that the permission engine uses. |
| [Permissions](/v0/concepts/permissions) | A five-layer intersection that decides whether a capability call is allowed, confirmed, or denied. |

## How they connect

At startup the runtime evaluates `init.lua`, which registers tools, actions, runners, skills, and services. The daemon then opens three endpoints — `/health`, `/ws`, and `/control` — and waits for client calls.

When a call arrives, the permission engine intersects the grants, action requirements, allowlists, and policy to reach a decision. Approved calls receive a `ctx` handle scoped to the capabilities they need. Results are returned to the caller and written to the trace log.

## See also

- [What is agent.d?](/v0/guide/what-is-agentd)
- [How it works](/v0/guide/how-it-works)
- [Quick start](/v0/guide/quick-start)
- [Writing init.lua](/v0/writing/init)

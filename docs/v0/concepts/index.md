# Core Concepts

agent.d is built from a small set of named building blocks. Once you know what each one does and how they fit together, the rest of the docs are easier to follow.

## The building blocks

| Concept | What it is |
|---|---|
| [Runtime](/v0/concepts/runtime) | The daemon process that loads your components at startup and checks permissions on every call. |
| [Tools and Actions](/v0/concepts/tools-and-actions) | A tool is a named group of actions. An action is one operation you can call, named `tool.action`. |
| [Runners](/v0/concepts/runners) | Named AI agents: a model, a set of skills, and the actions they can call. |
| [Skills](/v0/concepts/skills) | Reusable blocks of instructions added to a runner's system prompt. |
| [Services](/v0/concepts/services) | Long-running background Lua tasks (gateways, pollers) with restart supervision. |
| [Memory and State](/v0/concepts/memory-and-state) | Durable key/value storage (`ctx.memory`) that survives restarts, and short-lived state (`ctx.state`) that resets on reload. |
| [Interfaces and Callers](/v0/concepts/interfaces-and-callers) | Interfaces are the ways clients connect (WebSocket today). Every call carries a caller identity that permission checks use. |
| [Permissions](/v0/concepts/permissions) | Denials first, then questions: how agent.d decides whether a call runs, waits for an approval, or is refused. |

## How they connect

At startup the runtime runs `init.lua`, which registers tools, actions, runners, skills, and services. The daemon then opens three endpoints (`/health`, `/ws`, and `/control`) and waits for client calls.

When a call arrives, agent.d checks the denials first, then the grants, the allowlists, and the action's requirements. A call that is not granted can become a question. An approved call gets a `ctx` handle limited to the capabilities it was granted. The result goes back to the caller and into the trace log.

## See also

- [What is agent.d?](/v0/guide/what-is-agentd)
- [How it works](/v0/guide/how-it-works)
- [Quick start](/v0/guide/quick-start)
- [Writing init.lua](/v0/writing/init)

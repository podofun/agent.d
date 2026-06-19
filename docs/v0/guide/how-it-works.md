# How it works

This page explains the agent.d request lifecycle, the daemon's network surface, and the daemon-loads-once-serves-many model that lets multiple clients share the same runtime without duplicating component definitions.

## Daemon endpoints

The daemon opens three endpoints on startup:

```text
init.lua + packages + skills
        |
        v
   daemon process
        |
        +-- GET /health    open liveness probe — returns "ok"
        +-- /ws            client data plane (WebSocket, bearer-token auth)
        +-- /control       privileged approval plane (WebSocket, separate admin token)
```

`/health` requires no authentication and is safe to hit from a load-balancer probe or readiness check. `/ws` is where clients call actions and run runners. `/control` is where an operator connects to approve or deny pending requests — it carries a separate admin bearer token.

The startup banner prints the Local, WS, and Control URLs and the counts of loaded actions, runners, services, and skills so you can confirm what was registered.

## Daemon-loads-once-serves-many

The daemon evaluates your Lua entry file (typically `init.lua`) once at startup. Every `agentd.tool`, `agentd.action`, `agentd.runner`, `agentd.skill`, and `agentd.service` call during that evaluation registers a component in the runtime. Once loaded, those components are available to every client that connects — no per-client boot cost, no duplicated state.

In development mode (`--watch`), the daemon watches `init.lua`, every file pulled in via `import()`, loaded skill `.md` sources, and `grants.toml`. When any of them changes, the runtime rebuilds in place. In-flight requests drain on the old runtime via an executor swap; durable memory and a connected approval operator survive the reload.

## The request lifecycle

A client sends a method call over the `/ws` WebSocket (for example, `actions.call` with `name = "git.status"`). The runtime processes it in five steps:

1. **Route to the owning tool.** The runtime looks up which registered tool owns the requested action. If no tool owns it, the call fails with `not_found`.

2. **Run the five-layer permission engine.** Before the handler runs, the runtime evaluates:

   ```
   tool/package grants
     ∩ action.requires
     ∩ runner.allow (if the caller is a runner)
     ∩ interface.allow (if the caller is an interface)
     ∩ policy
   = Decision
   ```

   This is a default-deny intersection. Every layer must permit the call. `grants.toml` is the only source of grants — a component's `requires` declaration states what it needs but never self-grants.

   If the decision is **deny** and the action has `confirm = true` (or a required grant is missing and approvals are enabled), the runtime sends an approval request to any connected operator on `/control`. On timeout (default 120 seconds) the request fails closed.

   Policy `deny_actions` and `deny_permissions` are hard denials and are never escalated to approval.

3. **Deliver a `ctx` handle to the action handler.** If the call is allowed, the runtime invokes the Lua handler with `(args, ctx)`. The `ctx` handle exposes the approved capabilities for that invocation: shell, filesystem, HTTP, WebSocket, secrets, durable memory, ephemeral state, model calls, and cross-component calls. Each capability call is re-checked against grants at the point of use — the handle does not pre-authorize everything upfront.

4. **Return the result to the client.** The handler's return value is serialized and sent back in the envelope `{ "id": …, "ok": true, "result": … }`. If the handler raises an error, the client receives `{ "ok": false, "code": "…", "error": "…" }`.

5. **Write a trace event.** Every call is appended to the trace log (`$XDG_STATE_HOME/agentd/trace.jsonl` by default) so you can inspect activity with `agentctl trace -f` or ship the JSONL to your observability stack.

## Caller identity

Every connection gets a session id (`ws-<n>`). The `session` and `user` fields in a call's params override the identity carried into `ctx.caller`, which lets a gateway service forward per-user context without opening separate connections.

Services running in the background share the same permission engine but identify as `{ service = "<name>" }` in `ctx.caller` and are governed by their own `[service.<name>]` grant section in `grants.toml`.

## See also

- [Concepts: runtime](/v0/concepts/runtime)
- [Concepts: permissions](/v0/concepts/permissions)
- [Security: grants](/v0/security/grants)
- [Reference: protocol](/v0/reference/protocol)

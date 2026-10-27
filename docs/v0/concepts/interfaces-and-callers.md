# Interfaces and Callers

An interface is a way for a client to connect to the daemon, such as your own web backend or a chat bridge. Every call carries a caller identity that says who made it, and agent.d uses that identity when it decides whether the call may run.

## Interfaces

The only interface type supported today is WebSocket. Clients connect to `/ws` with a bearer token and then send requests as JSON messages:

```json
{ "id": 1, "method": "actions.call", "params": { "name": "git.status", "args": {} } }
```

Each WebSocket connection gets a generated session id (`ws-1`, `ws-2`, and so on).

The connection's token decides which interface it is. The daemon token identifies interface `ws`, and a token configured under `[interfaces.<name>]` identifies interface `<name>`. Clients that speak the same WebSocket protocol can then have their own `[interface.<name>]` grants and their own sessions. See [`[interfaces.<name>]`](/v0/reference/configuration#interfaces-name-sections).

To limit which actions an interface may call, add an `allowed_actions` entry to `grants.toml`:

```toml
[interface.telegram]
allowed_actions = ["git.status"]
```

A call from this interface to any other action is refused. The call still has to pass every other check. See [Permissions](/v0/concepts/permissions).

::: info
The [Protocol reference](/v0/reference/protocol) documents the full WebSocket protocol: methods, message format, authentication and error codes.
:::

## Callers

Every call, whether it comes from a WebSocket client, a runner's tool call, a service or another action, carries a caller that identifies who made it:

```lua
-- Available in any handler or service body (no permission required)
local c = ctx.caller
-- c.interface  string|nil  -- interface name: "ws" for the daemon token, else the [interfaces.<name>] that matched
-- c.runner     string|nil  -- runner name, if called from a runner tool-use step
-- c.service    string|nil  -- service name, if called from a service
-- c.session    string|nil  -- WebSocket session id or logical override (e.g. "ws-1")
-- c.user       string|nil  -- logical user id (override)
-- c.execution  string|nil  -- unique id for this invocation
```

agent.d uses the caller to decide which allowlists apply:

- A call from a runner is checked against the runner's `allowed_actions`. A missing action leads to a question.
- A call from an interface is checked against the interface's `allowed_actions`. A missing action is refused.
- A call from a service uses the service's `granted` capabilities and its optional `allowed_actions`.

## Session and user overrides

A client that serves its own users, such as a Telegram bot that maps chat users to agent sessions, can pass `session` and `user` parameters in the call:

```json
{ "id": 1, "method": "actions.call", "params": {
  "name": "git.status", "args": {},
  "session": "telegram-chat-42",
  "user": "alice"
}}
```

The `session` parameter replaces the generated id in `ctx.caller.session`, and `user` sets `ctx.caller.user`. The daemon trusts the interface to identify its users, so the bridge must authenticate each user before passing their id. Chat sessions are scoped by interface and by this `user` (see [`ctx.sessions`](/v0/reference/ctx/sessions)). Handlers can also read them to scope memory namespaces or apply per-user logic:

```lua
handler = function(args, ctx)
  local ns = "data/" .. (ctx.caller.user or ctx.caller.session or "anon")
  local mem = ctx.memory.create(ns)
  -- ...
end
```

`runners.run` accepts the same `session` and `user` params on the wire.

## Caller types and allowlists

| Caller type | Runner allowlist | Interface allowlist | Capability source |
|---|---|---|---|
| WebSocket client | Not used | `[interface.<id>].allowed_actions` | Not used |
| Runner tool call | `[runner.<name>].allowed_actions` | Inherited from the interface | Not used |
| Service | Not used | Not used | `[service.<name>].granted` + `allowed_actions` |
| `ctx.call` (action to action) | Inherited | Inherited | Tool grants |

## See also

- [ctx.caller reference](/v0/reference/ctx/caller)
- [Protocol reference](/v0/reference/protocol)
- [Permissions](/v0/concepts/permissions)
- [Tutorial: calling actions](/v0/tutorial/calling)

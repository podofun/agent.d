# Interfaces and Callers

An interface is a client surface connected to the daemon. Every call that arrives through an interface carries a Caller identity that the permission engine uses when making access decisions.

## Interfaces

The only interface type supported today is WebSocket. Clients connect to `/ws` with a bearer token and then send JSON-envelope requests:

```json
{ "id": 1, "method": "actions.call", "params": { "name": "git.status", "args": {} } }
```

Each WebSocket connection receives an auto-generated session id (`ws-1`, `ws-2`, …).

You can restrict which actions a given interface may invoke by adding an `allowed_actions` entry to `grants.toml`:

```toml
[interface.telegram]
allowed_actions = ["git.status"]
```

This adds an interface-level allowlist — permission layer 4. Only calls that pass this layer (and all other layers) are dispatched.

::: info
The full WebSocket protocol — methods, envelopes, auth, and error codes — is documented in the [Protocol reference](/v0/reference/protocol).
:::

## Callers

Every inbound call — whether from a WebSocket client, a runner tool-use step, a service, or another action — carries a Caller struct that identifies who triggered it:

```lua
-- Available in any handler or service body (no permission required)
local c = ctx.caller
-- c.interface  string|nil  -- interface name (always "ws" for WebSocket connections)
-- c.runner     string|nil  -- runner name, if called from a runner tool-use step
-- c.service    string|nil  -- service name, if called from a service
-- c.session    string|nil  -- WebSocket session id or logical override (e.g. "ws-1")
-- c.user       string|nil  -- logical user id (override)
-- c.execution  string|nil  -- unique id for this invocation
```

The permission engine consults `ctx.caller` to determine which allowlist layers to apply:
- A call from a runner adds the runner's `allowed_actions` as layer 3.
- A call from a WebSocket interface adds the interface's `allowed_actions` as layer 4.
- A call from a service uses the service's `granted` capabilities and optional `allowed_actions`.

## Session and user overrides

When a client bridges an external identity space (e.g. a Telegram bot mapping chat users to agent sessions), it can pass `session` and `user` parameters in the call:

```json
{ "id": 1, "method": "actions.call", "params": {
  "name": "git.status", "args": {},
  "session": "telegram-chat-42",
  "user": "alice"
}}
```

These override the auto-generated WebSocket session id in `ctx.caller.session` and `ctx.caller.user`. Handlers can read them to scope memory namespaces or apply per-user logic:

```lua
handler = function(args, ctx)
  local ns = "data/" .. (ctx.caller.user or ctx.caller.session or "anon")
  local mem = ctx.memory.create(ns)
  -- ...
end
```

`runners.run` accepts the same `session` and `user` params on the wire.

## Caller types and permission layers

| Caller type | Layer 3 (runner allow) | Layer 4 (interface allow) | Capability source |
|---|---|---|---|
| WebSocket client | — | `[interface.<id>].allowed_actions` | — |
| Runner tool-use step | `[runner.<name>].allowed_actions` | inherited from interface | — |
| Service | — | — | `[service.<name>].granted` + `allowed_actions` |
| `ctx.call` (action-to-action) | inherited | inherited | tool grants |

## See also

- [ctx.caller reference](/v0/reference/ctx/caller)
- [Protocol reference](/v0/reference/protocol)
- [Permissions](/v0/concepts/permissions)
- [Tutorial: calling actions](/v0/tutorial/calling)

# Recipe: Webhook trigger

Trigger an action from an external system using the agent.d WebSocket data plane. Clients connect to `/ws`, send an `actions.call` message, and optionally pass `session` and `user` to carry caller identity — which the action handler can read back via `ctx.caller`.

::: info Transport is WebSocket
agent.d does not have a built-in HTTP webhook endpoint. The data plane is `/ws` (WebSocket). If your external system can only speak HTTP, put a thin adapter in front that upgrades the connection and forwards the JSON envelope. The protocol is documented at [/reference/protocol](/v0/reference/protocol).
:::

## The protocol envelope

Every call over `/ws` uses this JSON envelope:

```json
// Client → server
{
  "id": 1,
  "method": "actions.call",
  "params": {
    "name": "notify.ingest",
    "args": { "event": "push", "repo": "acme/api", "sha": "abc123" },
    "session": "gh-webhook",
    "user": "github-actions"
  }
}

// Server → client (success)
{ "id": 1, "ok": true, "result": { "result": { "queued": true }, "duration_ms": 4 } }

// Server → client (error)
{ "id": 1, "ok": false, "code": "not_found", "error": "action `x` not registered" }
```

The `session` and `user` fields are optional. When present, they override the default identity (`ws-<n>` session, no user) and flow into `ctx.caller` inside the handler.

## Example action

This action receives an inbound event and reads back the caller's identity:

```lua
-- tools/notify.lua
agentd.tool({ name = "notify" })

agentd.action({
  name = "notify.ingest",
  handler = function(args, ctx)
    local caller = ctx.caller
    ctx.log.info(("notify.ingest from session=%s user=%s"):format(
      tostring(caller.session),
      tostring(caller.user)
    ))

    -- args carries whatever the caller sent in "args"
    ctx.log.info(("event=%s repo=%s"):format(
      tostring(args.event),
      tostring(args.repo)
    ))

    -- do work here: write to memory, call a runner, etc.
    return { queued = true }
  end,
})
```

`ctx.caller` is a read-only table with these fields:

| Field | Set when |
|---|---|
| `caller.session` | Always set for WebSocket connections (`ws-<n>`, or the `session` param if provided) |
| `caller.user` | Set when the caller passes `user` in the envelope |
| `caller.interface` | Set when the call arrives via a named interface |
| `caller.runner` | Set when the call is made from inside a runner via `ctx.call` |
| `caller.service` | Set when the call is made from a service via `ctx.call` |
| `caller.execution` | Set for runner executions |

## Entry point

```lua
-- init.lua
import("tools/notify.lua")
```

## grants.toml

`notify.ingest` does not require any capability grants in this minimal form. Add grants as needed when the handler calls out to memory, net, shell, etc.:

```toml
# No grants required for the bare action above.
# Add sections as you extend the handler, e.g.:
# [tool.notify]
# granted = ["memory.write:events/**"]
```

## How to run

```bash [release]
agentd --init init.lua --grants grants.toml
```

```bash [cargo]
cargo run -p daemon -- --init init.lua --grants grants.toml
```

## Sending a trigger from a WebSocket client

The daemon listens on `ws://127.0.0.1:7777/ws`. Auth is a bearer token sent in the `Authorization` header on the handshake (the auto-minted token is at `$XDG_STATE_HOME/agentd/token`).

With `agentctl` (which handles auth automatically):

```bash
agentctl call notify.ingest \
  -d event=push \
  -d repo=acme/api \
  -d sha=abc123 \
  --result-only
```

From any WebSocket client that can set headers (e.g. `websocat`):

```bash
TOKEN=$(cat ~/.local/state/agentd/token)
echo '{"id":1,"method":"actions.call","params":{"name":"notify.ingest","args":{"event":"push","repo":"acme/api","sha":"abc123"},"session":"gh-webhook","user":"github-actions"}}' \
  | websocat -H "Authorization: Bearer $TOKEN" ws://127.0.0.1:7777/ws
```

::: tip No-auth mode for local development
Pass `--no-auth` to the daemon (or set `no_auth = true` in `config.toml`) to skip bearer-token enforcement on `/ws`. Use only for local testing — never in production.
:::

## Reading caller identity in a handler

The `ctx.caller` table lets you gate behavior on who is calling:

```lua
handler = function(args, ctx)
  if ctx.caller.user ~= "github-actions" then
    error("unexpected caller: " .. tostring(ctx.caller.user))
  end
  -- proceed
end,
```

This is not a security boundary by itself — the `user` field is caller-supplied and only as trustworthy as your client. For hard security boundaries, use the permission engine's grant layers.

## Verify

```bash
agentctl call notify.ingest -d event=test -d repo=my/repo
agentctl trace -n 5
```

The trace log should show the `notify.ingest` invocation with the caller session.

## See also

- [WebSocket protocol reference](/v0/reference/protocol)
- [ctx.caller reference](/v0/reference/ctx/caller)
- [Concepts: interfaces and callers](/v0/concepts/interfaces-and-callers)
- [Security: grants](/v0/security/grants)

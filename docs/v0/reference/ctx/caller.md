# ctx.caller — Caller Identity

`ctx.caller` is a read-only table that describes who or what triggered the current invocation. Use it to implement per-user routing, audit logging, or caller-specific behaviour.

**Required permission:** none. All handlers and services can read `ctx.caller` freely.

## Signature

```lua
ctx.caller -> {
  interface?: string,
  runner?:    string,
  service?:   string,
  session?:   string,
  user?:      string,
  execution?: string,
}
```

All fields are `string | nil`. You should check for `nil` before using any field — the value is only present when it was set by the connection or call context.

## Fields

| Field | Type | Description |
|---|---|---|
| `interface` | `string \| nil` | The interface name the call arrived on (e.g. the WebSocket connection label). |
| `runner` | `string \| nil` | Name of the runner that is making this tool call, if the action was invoked from a runner loop. |
| `service` | `string \| nil` | Name of the service that called `ctx.call`, if applicable. |
| `session` | `string \| nil` | Session identifier. Defaults to `ws-<n>` (the connection sequence number) and can be overridden by the client via the `session` field in `actions.call` or `runners.run`. |
| `user` | `string \| nil` | User identifier. `nil` unless the client sets `user` in the call params. |
| `execution` | `string \| nil` | Unique identifier for this invocation. |

## How identity is set

Every WebSocket connection on `/ws` receives an auto-assigned session id `ws-<n>`. Clients can override `session` and `user` per-request:

```json
{ "id": 1, "method": "actions.call", "params": { "name": "git.status", "session": "alice", "user": "alice@example.com" } }
```

Inside the handler, `ctx.caller.session` is `"alice"` and `ctx.caller.user` is `"alice@example.com"`.

See [Concepts: interfaces and callers](/v0/concepts/interfaces-and-callers) for the full identity model.

## Examples

```lua
-- Log the caller on every invocation
agentd.action("git.status", function(args, ctx)
  ctx.log.info("called by session=" .. (ctx.caller.session or "?") ..
               " user=" .. (ctx.caller.user or "anonymous"))
  return ctx.shell("git", { "status", "--short" }).stdout
end)
```

```lua
-- Per-user memory namespace
agentd.action("notes.get", function(args, ctx)
  local user = ctx.caller.user
  if not user then
    error("user identity required")
  end
  local mem = ctx.memory.create("notes/" .. user)
  return mem:get(args.key)
end)
```

```lua
-- Route to different runners based on the calling runner
agentd.action("ai.dispatch", function(args, ctx)
  if ctx.caller.runner == "backend_reviewer" then
    return ctx.run("code_fixer", args.prompt)
  end
  return ctx.run("default_runner", args.prompt)
end)
```

## See also

- [Concepts: interfaces and callers](/v0/concepts/interfaces-and-callers)
- [Reference: protocol](/v0/reference/protocol)
- [ctx.memory — per-user namespaces](/v0/reference/ctx/memory)
- [Recipes: per-user-memory](/v0/recipes/per-user-memory)

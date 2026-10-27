# ctx.caller: Caller Identity

Use `ctx.caller` when a handler needs to know who started it, for example to route per user, to write an audit log, or to keep state per chat. It is a read-only table that describes who or what triggered the current call.

It needs no permission. Every handler and service can read it.

## Signature

```lua
ctx.caller -> {
  interface?: string,
  runner?:    string,
  service?:   string,
  session?:   string,
  session_id?: string,
  run_id?:    string,
  user?:      string,
  execution?: string,
}
```

Every field is a string or `nil`. A field is set only when the connection or the call provides it, so check for `nil` before you use one.

## Fields

| Field | Type | Description |
|---|---|---|
| `interface` | `string \| nil` | The interface name the call arrived on, for example the WebSocket connection label. |
| `runner` | `string \| nil` | Name of the runner making this tool call, when a runner called the action. |
| `service` | `string \| nil` | Name of the service that called `ctx.call`, when a service made the call. |
| `session` | `string \| nil` | Connection identifier. It defaults to `ws-<n>` (the connection sequence number), and a client can set its own with the `session` field in `actions.call` or `runners.run`. |
| `session_id` | `string \| nil` | The [chat session](/v0/reference/ctx/sessions) the run belongs to. It is set when the run was started with a `session_id`. Use it to keep state per chat. |
| `run_id` | `string \| nil` | The run this call is part of. It is set for calls a runner makes. |
| `user` | `string \| nil` | User identifier. `nil` unless the client sets `user` in the call params. |
| `execution` | `string \| nil` | A unique identifier for this call. |

## How identity is set

Each WebSocket connection on `/ws` gets a session id of the form `ws-<n>`. A client can set `session` and `user` on each request:

```json
{ "id": 1, "method": "actions.call", "params": { "name": "git.status", "session": "alice", "user": "alice@example.com" } }
```

Inside the handler, `ctx.caller.session` is `"alice"` and `ctx.caller.user` is `"alice@example.com"`.

See [Concepts: interfaces and callers](/v0/concepts/interfaces-and-callers) for the full identity model.

## Sessions and runs

`session_id` and `run_id` tell you which conversation and which run a call belongs to. They are different from `session`, which names the connection.

Each time a runner starts, agent.d gives the run a new `run_id`, such as `run-12`. Every tool call the runner makes during that run carries the same `run_id`.

When the run was started with a `session_id`, its tool calls carry that id too. State you key by `session_id` therefore lasts across every run in the chat, while state keyed by `run_id` covers one run. A run that a handler starts with [`ctx.run`](/v0/reference/ctx/calls#ctx-run-name-prompt-or-opts) gets its own `run_id`, and it has a `session_id` only when you pass one.

[`ctx.fs.edit` and `ctx.fs.patch`](/v0/reference/ctx/fs#changing-part-of-a-file) use these ids to check that a file was read before it is changed.

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
- [ctx.memory: per-user namespaces](/v0/reference/ctx/memory)
- [Recipes: per-user-memory](/v0/recipes/per-user-memory)

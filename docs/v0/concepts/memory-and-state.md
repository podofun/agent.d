# Memory and State

agent.d gives you two storage mechanisms: durable `ctx.memory` that survives restarts and reloads, and ephemeral `ctx.state` that is scoped to the current runtime and lost on reload. Knowing when to use each one keeps your components predictable.

## Durable memory (`ctx.memory`)

`ctx.memory` is an embedded key/value store on disk. Data written here is preserved across daemon restarts and hot reloads.

```lua
-- Required permissions: memory.read:<namespace> / memory.write:<namespace>
local mem = ctx.memory.create("discord/chan/" .. channel_id)

-- Write
mem:set("log", history)

-- Read (with optional default)
local log = mem:get("log") or {}

-- Other operations
mem:exists("log")    -- boolean
mem:keys()           -- string[]
mem:delete("log")
mem:clear()          -- remove all keys in this namespace
```

Memory is **namespaced**: `ctx.memory.create(namespace)` returns a handle scoped to that namespace string. Namespaces support hierarchical patterns — `"discord/chan/12345"` is a child of `"discord/**"`. The permission slugs match this hierarchy:

```toml
[service.discord_handler]
granted = [
  "memory.read:discord/**",
  "memory.write:discord/**",
]
```

This allows the service to read and write any key under any `discord/...` namespace.

Values are JSON-serialized automatically. You can store strings, numbers, booleans, tables, and arrays. `json.null` is preserved correctly.

Memory lives under `$XDG_DATA_HOME/agentd/memory.redb`.

## Ephemeral state (`ctx.state`)

`ctx.state` is an in-process key/value store. It requires no permission grants and is fast, but it is **lost whenever the runtime is rebuilt** — on hot reload or daemon restart.

```lua
-- No permission required
ctx.state.set("bot_user_id", ev.d.user.id)
local id = ctx.state.get("bot_user_id")

ctx.state.delete("bot_user_id")
ctx.state.keys()    -- string[]
ctx.state.clear()
```

Use `ctx.state` for:
- Caching values computed at startup (e.g. a bot's own user ID from the READY event).
- Short-lived coordination between actions within the same runtime lifetime.

## Choosing between them

| | `ctx.memory` | `ctx.state` |
|---|---|---|
| Survives restart | yes | no |
| Survives hot reload | yes | no |
| Permission required | `memory.read/write:<ns>` | none |
| Storage | on disk | in-process |
| Use for | user data, history, configuration | temporary cache, in-session flags |

::: tip
When in doubt, use `ctx.memory`. The overhead is minimal and you avoid silent data loss on reload.
:::

## See also

- [ctx.memory reference](/v0/reference/ctx/memory)
- [Services](/v0/concepts/services)
- [Per-user memory recipe](/v0/recipes/per-user-memory)

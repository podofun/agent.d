# Memory and State

agent.d gives you two places to keep data. `ctx.memory` is durable and survives restarts and reloads. `ctx.state` lives in the running process and is cleared on each reload. This page helps you choose between them.

## Durable memory (`ctx.memory`)

`ctx.memory` is a key/value store on disk. Data you write here stays across daemon restarts and hot reloads.

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

Memory is split into **namespaces**. `ctx.memory.create(namespace)` returns a handle limited to that namespace. Namespaces form a hierarchy: `"discord/chan/12345"` sits under `"discord/**"`. Permission slugs follow the same hierarchy:

```toml
[service.discord_handler]
granted = [
  "memory.read:discord/**",
  "memory.write:discord/**",
]
```

This allows the service to read and write any key under any `discord/...` namespace.

Values are stored as JSON for you. You can store strings, numbers, booleans, tables, and arrays, and `json.null` comes back as `json.null`.

Memory lives under `$XDG_DATA_HOME/agentd/memory.redb`.

## Ephemeral state (`ctx.state`)

`ctx.state` is a key/value store in the daemon's memory. It needs no permission grants and is fast. Its contents are **cleared whenever the runtime is rebuilt**, on a hot reload or a daemon restart.

```lua
-- No permission required
ctx.state.set("bot_user_id", ev.d.user.id)
local id = ctx.state.get("bot_user_id")

ctx.state.delete("bot_user_id")
ctx.state.keys()    -- string[]
ctx.state.clear()
```

Use `ctx.state` for:
- Caching values computed at startup (for example a bot's own user ID from the READY event).
- Short-lived coordination between actions within the same runtime lifetime.

## Choosing between them

| | `ctx.memory` | `ctx.state` |
|---|---|---|
| Survives restart | yes | no |
| Survives hot reload | yes | no |
| Permission required | `memory.read/write:<ns>` | none |
| Storage | `memory.redb` on disk | daemon memory |
| Use for | user data, history, configuration | temporary cache, in-session flags |

::: tip
When in doubt, use `ctx.memory`. It costs little, and your data is still there after a reload.
:::

## See also

- [ctx.memory reference](/v0/reference/ctx/memory)
- [Services](/v0/concepts/services)
- [Per-user memory recipe](/v0/recipes/per-user-memory)

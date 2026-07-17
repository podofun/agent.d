# ctx.memory / ctx.state — Memory and State

agent.d provides two key/value stores: **durable memory** (`ctx.memory`) that survives restarts and hot reloads, and **ephemeral state** (`ctx.state`) that exists only for the lifetime of the current runtime process.

## Durable memory — `ctx.memory`

`ctx.memory` is backed by an embedded database stored at `$XDG_DATA_HOME/agentd/memory.redb`. Values are JSON-serialized automatically.

**Required permissions:** `memory.read:<ns-glob>` for reads; `memory.write:<ns-glob>` for writes and deletes.

### Signatures

```lua
ctx.memory.create(namespace: string) -> Handle

-- Handle methods:
handle:get(key: string, default?: any) -> any
handle:set(key: string, value: any)
handle:exists(key: string) -> boolean
handle:keys() -> string[]
handle:delete(key: string)
handle:clear()
```

### Handle methods

| Method | Description |
|---|---|
| `:get(key, default?)` | Return the stored value for `key`, or `default` (or `nil`) if not present. |
| `:set(key, value)` | Store `value` under `key`. Any JSON-serializable value is accepted. |
| `:exists(key)` | Return `true` if `key` is present. |
| `:keys()` | Return all keys in this namespace as `string[]`. |
| `:delete(key)` | Remove `key` from the namespace. |
| `:clear()` | Remove all keys from the namespace. |

### Permission

Namespaces support glob matching in grants:

```toml
[service.discord_handler]
granted = [
  "memory.read:discord/**",
  "memory.write:discord/**",
]
```

A single `ctx.memory.create` call opens one namespace. You can open multiple namespaces in the same action or service.

### Example

```lua
-- Per-channel conversation memory in a Discord bot
agentd.service("discord_handler", function(ctx)
  local mem = ctx.memory.create("discord/channels")

  -- load history for a channel
  local history = mem:get("history:" .. channel_id, {})

  -- append a message
  table.insert(history, { role = "user", content = message })
  mem:set("history:" .. channel_id, history)
end)
```

---

## Ephemeral state — `ctx.state`

`ctx.state` is an in-process key/value map shared across all calls in the current runtime lifetime. It is **lost** on daemon restart or hot reload (`--watch`). Use it for caches, counters, or coordination data that can be rebuilt on startup.

**Required permission:** none.

### Signatures

```lua
ctx.state.get(key: string, default?: any) -> any
ctx.state.set(key: string, value: any)
ctx.state.delete(key: string)
ctx.state.keys() -> string[]
ctx.state.clear()
```

### Methods

| Method | Description |
|---|---|
| `ctx.state.get(key, default?)` | Return the value for `key`, or `default` (or `nil`) if not present. |
| `ctx.state.set(key, value)` | Store `value` under `key`. Any Lua value is accepted. |
| `ctx.state.delete(key)` | Remove `key`. |
| `ctx.state.keys()` | Return all stored keys as `string[]`. |
| `ctx.state.clear()` | Remove all keys. |

### Example

```lua
-- In-memory request counter (resets on reload)
agentd.action("metrics.hits", function(args, ctx)
  local n = ctx.state.get("hits", 0) + 1
  ctx.state.set("hits", n)
  return { hits = n }
end)
```

---

## Choosing between memory and state

| | `ctx.memory` | `ctx.state` |
|---|---|---|
| **Survives restart** | yes | no |
| **Survives hot reload** | yes | no |
| **Permission required** | `memory.read/write:<ns>` | none |
| **Value encoding** | JSON-serialized | native Lua values |
| **Best for** | User data, conversation history, config | Caches, counters, ephemeral coordination |

## See also

- [Concepts: memory and state](/v0/concepts/memory-and-state)
- [Security: permission slugs](/v0/security/permission-slugs)
- [Recipes: per-user-memory](/v0/recipes/per-user-memory)
- [Recipes: discord-bot](/v0/recipes/discord-bot)

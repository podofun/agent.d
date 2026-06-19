# Services

A service is a long-running background Lua task managed by the daemon. Use services for things that need to run continuously without a client trigger — event gateways, pollers, message queue consumers, or periodic jobs.

## Defining a service

```lua
agentd.service("discord_gateway", { restart = "always", backoff_ms = 5000 }, function(ctx)
  -- runs for the lifetime of the daemon (or until it errors)
  local ws = ctx.ws.connect("wss://gateway.discord.gg/?v=10&encoding=json")
  ws:each(function(frame)
    -- handle each frame
  end)
end)
```

The signature:

```lua
agentd.service(name, body)
agentd.service(name, opts, body)
```

`opts` controls restart supervision:

| Option | Default | Meaning |
|---|---|---|
| `restart` | — | `"always"` restarts on any exit; `"on_failure"` restarts only on error. |
| `backoff_ms` | — | Initial delay before the first restart (milliseconds). |
| `backoff_max_ms` | — | Maximum backoff delay (milliseconds). |

When `restart` is omitted, the service runs once and is not restarted.

## Supervision and restarts

The daemon's supervisor starts each service in its own coroutine. If the service body returns or throws, the supervisor checks the `restart` policy:

- `"always"` — restart unconditionally, with the configured backoff.
- `"on_failure"` — restart only if the body threw an error.
- no policy — the service exits permanently.

Backoff applies between restarts. If both `backoff_ms` and `backoff_max_ms` are set, the delay grows up to the maximum.

## Services as callers

A service is a first-class caller. It has its own grant section in `grants.toml`:

```toml
[service.discord_handler]
granted         = ["ai:openai", "net:discord.com", "memory.read:discord/**", "memory.write:discord/**"]
allowed_actions = ["discord.send"]
```

`granted` lists the capability slugs the service's `ctx` handle is allowed to use. `allowed_actions` restricts which other actions the service may invoke via `ctx.call`. Both are checked by the permission engine on every call — the service does not bypass layers.

## ctx in a service

The `ctx` handle is the first (and only) argument to the service body:

```lua
agentd.service("my_poller", { restart = "always", backoff_ms = 2000 }, function(ctx)
  while true do
    local data = ctx.http.get("https://api.example.com/events")
    -- process data...
    sleep(10000)
  end
end)
```

Services have access to the full `ctx` API — shell, filesystem, HTTP, WebSocket, secrets, memory, AI calls, and inter-component calls — subject to their `granted` slugs.

## Durable state across reloads

Services are restarted on hot reload because the Lua runtime is rebuilt. To preserve state across reloads, write it to `ctx.memory` (durable, redb-backed). The data survives both restarts and reloads.

Use `ctx.state` only for in-session bookkeeping that you don't need to survive a reload.

## Listing services

```bash [release]
agentctl services ls
```

```bash [cargo]
cargo run -p agentd-cli -- services ls
```

## See also

- [Writing services](/v0/writing/services)
- [Memory and state](/v0/concepts/memory-and-state)
- [Discord bot recipe](/v0/recipes/discord-bot)

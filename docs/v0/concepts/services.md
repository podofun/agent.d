# Services

A service is a long-running Lua task that the daemon runs in the background. Use a service for work that runs on its own, with no client call to start it: event gateways, pollers, message queue consumers, or periodic jobs.

## Defining a service

You register a service with `agentd.service`:

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

`opts` controls whether and how the service restarts:

| Option | Default | Meaning |
|---|---|---|
| `restart` | none | `"always"` restarts on any exit. `"on_failure"` restarts only on error. |
| `backoff_ms` | 1000 | Delay before the first restart (milliseconds). |
| `backoff_max_ms` | 60000 | Longest delay between restarts (milliseconds). |

When `restart` is omitted, the service runs once and is not restarted.

## Supervision and restarts

The daemon starts each service in its own coroutine and watches it. When the service body returns or raises an error, the daemon checks the `restart` setting:

- `"always"`: restart, after the delay.
- `"on_failure"`: restart only if the body raised an error.
- Not set: the service stays stopped.

The delay starts at `backoff_ms` and doubles after each failure in a row, up to `backoff_max_ms`.

## Services as callers

A service makes calls in its own name, like a client or a runner. It has its own grant section in `grants.toml`:

```toml
[service.discord_handler]
granted         = ["ai:openai", "net:discord.com", "memory.read:discord/**", "memory.write:discord/**"]
allowed_actions = ["discord.send"]
```

`granted` lists the capability slugs the service's `ctx` handle may use. `allowed_actions` limits which actions the service may call with `ctx.call`. agent.d checks both on every call, with the same checks as any other caller.

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

Services can use the full `ctx` API (shell, filesystem, HTTP, WebSocket, secrets, memory, AI calls, and calls to other actions), limited by their `granted` slugs.

## Durable state across reloads

A hot reload rebuilds the Lua runtime, so services restart with it. To keep data across reloads and restarts, write it to `ctx.memory`, which is stored on disk.

Use `ctx.state` for short-lived bookkeeping that can be lost on a reload.

## Listing services

To see each service and its state, run:

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

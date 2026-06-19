# Writing Services

A **service** is a long-running background Lua task supervised by the daemon.
Use services for persistent connections (WebSocket gateways), polling loops,
or any work that should run continuously alongside your tools and runners.

## Registration

### Simple form

```lua
agentd.service("my_poller", function(ctx)
  -- body runs once; ctx is the per-invocation capability handle
  while true do
    ctx.log.info("polling…")
    sleep(30000)
  end
end)
```

### With supervisor options

```lua
agentd.service("discord_gateway", {
  restart      = "always",      -- "always" | "on_failure"
  backoff_ms   = 5000,          -- initial reconnect delay
  backoff_max_ms = 60000,       -- cap on exponential back-off
}, function(ctx)
  -- body
end)
```

`opts` fields:

| Field | Type | Default | Description |
|---|---|---|---|
| `restart` | `"always" \| "on_failure"` | — | When to restart. `"always"` restarts even on a clean return. |
| `backoff_ms` | `int` | — | Initial delay in milliseconds before the first restart. |
| `backoff_max_ms` | `int` | — | Maximum delay after repeated failures (exponential back-off). |

## The service body

```lua
function(ctx)
  -- ctx is the per-invocation capability handle
  -- (1st argument to service bodies; 2nd argument to action handlers)
end
```

The body runs in its own coroutine. When it returns or errors, the supervisor
restarts it according to the `restart` policy. Long-running services typically
contain an infinite loop driven by a channel receive, a WebSocket read, or a
timer.

## Long-running patterns

### WebSocket gateway with timer heartbeat

```lua
agentd.service("discord_gateway", { restart = "always", backoff_ms = 5000 }, function(ctx)
  -- permission: net:gateway.discord.gg
  local ws = ctx.ws.connect("wss://gateway.discord.gg/?v=10&encoding=json")

  -- permission: none (timer is a global helper)
  timer.every(41250, function()
    if not ws:is_closed() then
      ws:send(json.encode({ op = 1, d = json.null }))
    end
  end)

  local events = channel("discord_events")   -- named process-wide channel

  ws:each(function(frame)
    if frame.kind == "text" then
      local ev = json.decode(frame.text)
      if ev.op == 0 and ev.t == "MESSAGE_CREATE" then
        events:send(ev.d)
      end
    end
  end)

  ctx.log.warn("gateway loop exited; supervisor will reconnect")
end)
```

### Event consumer with async dispatch

```lua
agentd.service("discord_handler", { restart = "always" }, function(ctx)
  local events = channel("discord_events")   -- same named channel

  while true do
    local msg = events:recv()
    if msg == nil then return end

    -- handle each message asynchronously so the recv loop stays hot
    async(function()
      -- permission: ai:openai (via ctx.run)
      local result = ctx.run("discord_chat", { prompt = msg.content })
      -- permission: net:discord.com (via ctx.call)
      ctx.call("discord.send", { channel_id = msg.channel_id, content = result.text })
    end)
  end
end)
```

## Channels and timers

Services can use the process-wide named channel to coordinate with each other:

```lua
local ch = channel("my_events")   -- create or retrieve by name
ch:send({ type = "ping" })
local msg = ch:recv()             -- blocks until a message arrives
local msg = ch:try_recv()         -- returns nil immediately if empty
```

Timers fire callbacks on a background coroutine:

```lua
timer.after(5000, function() ctx.log.info("once after 5 s") end)
timer.every(10000, function() ctx.log.info("every 10 s") end)
```

See [Concurrency reference](/v0/reference/ctx/concurrency) for `async`, `await`,
`parallel`, and the full Channel API.

## Permissions — `[service.<name>]` grants

Services have their own grant section in `grants.toml`. A service's body cannot
use a capability unless that capability is listed under `granted`:

```toml
[service.discord_handler]
granted = [
  "ai:openai",               # ctx.run → model call
  "net:discord.com",         # ctx.call discord.send → HTTP
  "memory.read:discord/**",  # ctx.memory.create
  "memory.write:discord/**",
]
allowed_actions = ["discord.send"]
```

::: warning
Forgetting a grant causes the capability call to fail at runtime, not at load
time. Check the trace log (`agentctl trace -f`) if a service exits unexpectedly.
:::

## Ephemeral vs durable state

Inside a service body you have access to both:

| | API | Survives restart? | Permission |
|---|---|---|---|
| Ephemeral | `ctx.state.get/set` | No | none |
| Durable | `ctx.memory.create(ns)` | Yes | `memory.read:<ns>` / `memory.write:<ns>` |

The Discord example stores the bot's own user ID in `ctx.state` (lost on restart,
re-populated on the next READY event) and per-channel history in `ctx.memory`
(persists across restarts and hot reloads).

## See also

- [Services concept](/v0/concepts/services)
- [ctx overview](/v0/writing/context)
- [Concurrency reference](/v0/reference/ctx/concurrency)
- [Discord bot recipe](/v0/recipes/discord-bot)

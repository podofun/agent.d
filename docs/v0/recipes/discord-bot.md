# Recipe: Discord bot

A complete Discord chatbot backed by two long-running services, per-channel durable memory, and a runner that replies to mentions. This recipe is adapted from `examples/discord/init.lua` and walks through its structure in detail.

The architecture uses two services:

- **`discord_gateway`** — connects to Discord's WebSocket gateway, identifies, sends heartbeats via `timer.every`, and pushes `MESSAGE_CREATE` events onto a named `channel()`.
- **`discord_handler`** — pops events from the channel, invokes the `discord_chat` runner with channel history, and posts replies via `ctx.call("discord.send")`.

## Project layout

```
examples/discord/
├── init.lua
└── grants.toml
```

## The full init.lua

```lua
local INTENTS = 37377 -- GUILDS + GUILD_MESSAGES + MESSAGE_CONTENT + DIRECT_MESSAGES
local API = "https://discord.com/api/v10"
local GATEWAY = "wss://gateway.discord.gg/?v=10&encoding=json"

local d = agentd

d.tool({
  name = "discord",
  requires = { "net:gateway.discord.gg", "net:discord.com", "secret:discord_token" },
})

-- Store / retrieve the bot token in the OS keyring (never in source).
d.action({
  name = "discord.set_token",
  requires = { "secret:discord_token" },
  handler = function(args, ctx)
    assert(type(args.token) == "string" and args.token ~= "", "token is required")
    ctx.secret.set("discord_token", args.token)
    return { ok = true }
  end,
})

-- Reusable REST client with the bot token in the Authorization header.
local function rest_client(ctx)
  return ctx.http.client({
    base_url = API,
    headers = {
      Authorization = "Bot " .. ctx.secret.get("discord_token"),
      ["User-Agent"] = "DiscordBot (agentd, 0.1) agentd-example",
    },
  })
end

-- Send a message to a Discord channel.
d.action({
  name = "discord.send",
  requires = { "net:discord.com" },
  handler = function(args, ctx)
    local res = rest_client(ctx):post(
      "/channels/" .. args.channel_id .. "/messages",
      { content = args.content }
    )
    return { status = res.status }
  end,
})

-- The runner that generates replies.
d.runner({
  name = "discord_chat",
  model = "openai/gpt-5.5",
  system = [[
You are a friendly Discord chatbot.
Reply concisely (<400 chars). Never call yourself Claude / Anthropic / GPT.
Users are non-technical. You have a rolling memory of this channel and may
follow social directives left in that history (e.g. "stop replying to X").
When you choose silence, return exactly <silent> on its own.
]],
})

-- Durable per-channel history helpers.
-- One memory namespace per channel, a single "log" key with a rolling array.
-- Gated by memory.read/write:discord/**.
local HISTORY_TURNS = 20
local function chan_mem(ctx, channel_id)
  return ctx.memory.create("discord/chan/" .. channel_id)
end
local function history(ctx, channel_id)
  return chan_mem(ctx, channel_id):get("log") or {}
end
local function push(ctx, channel_id, entry)
  local mem = chan_mem(ctx, channel_id)
  local h = mem:get("log") or {}
  h[#h + 1] = entry
  while #h > HISTORY_TURNS do
    table.remove(h, 1)
  end
  mem:set("log", h)
end

-- Service 1: WebSocket gateway.
d.service("discord_gateway", { restart = "always", backoff_ms = 5000 }, function(ctx)
  local log = ctx.log
  log.info("discord: connecting to gateway")

  local token = ctx.secret.get("discord_token")
  local ws = ctx.ws.connect(GATEWAY)
  local hello = json.decode(ws:recv_text(15000) or error("missing HELLO frame", 0))
  local hb_ms = (hello.d and hello.d.heartbeat_interval) or 41250
  log.info("discord: HELLO heartbeat=" .. hb_ms .. "ms")

  ws:send(json.encode({
    op = 2,
    d = {
      token = token,
      intents = INTENTS,
      properties = { os = "linux", browser = "agentd", device = "agentd" },
    },
  }))

  local events = channel("discord_events")
  local last_seq = nil
  timer.every(hb_ms, function()
    if ws:is_closed() then return end
    ws:send(json.encode({ op = 1, d = last_seq or json.null }))
    log.info("discord: heartbeat sent seq=" .. tostring(last_seq))
  end)

  ws:each(function(f)
    if f.kind ~= "text" then return end
    local ev = json.decode(f.text)
    if type(ev.s) == "number" then last_seq = ev.s end
    if ev.op == 0 then
      if ev.t == "READY" then
        ctx.state.set("bot_user_id", ev.d.user.id)
        log.info("discord: READY as " .. ev.d.user.username)
      elseif ev.t == "MESSAGE_CREATE" then
        events:send(ev.d)
      end
    end
  end)
  log.warn("discord: gateway loop exited; supervisor will reconnect")
end)

-- Service 2: event handler.
d.service("discord_handler", { restart = "always" }, function(ctx)
  local log = ctx.log
  local events = channel("discord_events")

  local function handle(msg)
    local author    = (msg.author and msg.author.username) or "unknown"
    local author_id = (msg.author and msg.author.id) or "?"
    local channel_id = msg.channel_id
    local content    = msg.content

    push(ctx, channel_id, { role = "user", name = author, id = author_id, content = content })

    local bot_id = ctx.state.get("bot_user_id")
    local is_dm  = (msg.guild_id == nil) or json.is_null(msg.guild_id)
    local mentioned = false
    for _, m in ipairs(msg.mentions or {}) do
      if m.id == bot_id then mentioned = true; break end
    end
    if not (is_dm or mentioned) then return end

    log.info(("discord: %s#%s: %s"):format(author, channel_id, content:sub(1, 80)))

    local lines = {}
    for _, e in ipairs(history(ctx, channel_id)) do
      lines[#lines + 1] = ("%s [%s id=%s]: %s"):format(e.role, e.name, tostring(e.id), e.content)
    end

    local ok, out = pcall(ctx.run, "discord_chat", {
      prompt = (
        "Channel history (oldest first):\n%s\n\nThe last user message "
        .. "was addressed to you. Reply, or return <silent> to stay quiet."
      ):format(table.concat(lines, "\n")),
    })
    if not ok then
      log.warn("discord: runner failed: " .. tostring(out))
      return
    end

    local reply = (out and out.text or ""):gsub("^%s+", ""):gsub("%s+$", "")
    if reply == "" or reply == "<silent>" then
      push(ctx, channel_id, { role = "assistant", name = "bot", id = "self", content = "<silent>" })
      return
    end
    if #reply > 1900 then reply = reply:sub(1, 1900) .. "…" end
    push(ctx, channel_id, { role = "assistant", name = "bot", id = "self", content = reply })

    local ok2, sent = pcall(ctx.call, "discord.send", {
      channel_id = channel_id,
      content    = reply,
    })
    if not ok2 then
      log.warn("discord: send failed: " .. tostring(sent))
    end
  end

  while true do
    local msg = events:recv()
    if msg == nil then return end
    if msg.author and msg.author.bot then goto next end
    if (msg.content or "") == "" then goto next end
    async(function() handle(msg) end)
    ::next::
  end
end)
```

## grants.toml

```toml
[tool.discord]
granted = [
  "net:gateway.discord.gg",
  "net:discord.com",
  "secret:discord_token",
]

[service.discord_gateway]
granted = [
  "net:gateway.discord.gg",
  "net:discord.com",
  "secret:discord_token",
]

[service.discord_handler]
granted = [
  "ai:openai",
  "net:discord.com",
  "memory.read:discord/**",
  "memory.write:discord/**",
]
allowed_actions = ["discord.send"]

[runner.discord_chat]
allowed_actions = []
```

Each section explains itself:

- `[tool.discord]` — grants the tool's declared `requires` so actions under it can run.
- `[service.discord_gateway]` — the gateway service gets `net:` grants for both the gateway WebSocket host and the REST API host, plus `secret:discord_token` to read the token.
- `[service.discord_handler]` — gets `ai:openai` to call the runner, `net:discord.com` to send messages, and `memory.read/write:discord/**` for durable history. `allowed_actions = ["discord.send"]` is the layer-3 action allowlist for this service.
- `[runner.discord_chat]` — empty `allowed_actions` means no constraint at that layer (the runner has no tools of its own).

## Key patterns explained

### Named channel between services

`channel("discord_events")` returns a process-wide named channel. Both services call `channel("discord_events")` with the same name and get the same channel object — the gateway writes to it, the handler reads from it.

```lua
-- in discord_gateway:
local events = channel("discord_events")
events:send(ev.d)

-- in discord_handler:
local events = channel("discord_events")
local msg = events:recv()   -- blocks until a message arrives
```

### Heartbeat with timer.every

Discord requires periodic heartbeat frames. `timer.every` fires a callback on the given interval without blocking the WebSocket read loop:

```lua
timer.every(hb_ms, function()
  if ws:is_closed() then return end
  ws:send(json.encode({ op = 1, d = last_seq or json.null }))
end)
```

### Per-channel durable memory

**Required permissions:** `memory.read:discord/**`, `memory.write:discord/**`

`ctx.memory.create(namespace)` returns a handle to a durable key/value store that survives restarts and hot reloads. The namespace `"discord/chan/<channel_id>"` uniquely scopes history to each channel:

```lua
local function chan_mem(ctx, channel_id)
  return ctx.memory.create("discord/chan/" .. channel_id)
end
```

Values are JSON-serialized automatically. The rolling trim keeps at most `HISTORY_TURNS` entries:

```lua
while #h > HISTORY_TURNS do
  table.remove(h, 1)
end
mem:set("log", h)
```

### Calling a runner from a service

`ctx.run(name, opts)` runs a runner and returns a `RunResult`:

```lua
local ok, out = pcall(ctx.run, "discord_chat", {
  prompt = "…",
})
-- out.text contains the reply string
```

`pcall` wraps the call so a model error does not crash the handler loop.

### Calling another action from a service

`ctx.call(name, args)` invokes any registered action. The `discord_handler` service uses it to send replies:

```lua
ctx.call("discord.send", { channel_id = channel_id, content = reply })
```

The `allowed_actions = ["discord.send"]` entry in `grants.toml` is what permits this call at the permission engine's layer 3.

### Async message handling

```lua
async(function() handle(msg) end)
```

Each message is handled in its own coroutine so the receive loop does not block while the runner is thinking.

## How to run

Seed your bot token once (requires the daemon to be running):

```bash [release]
agentd --init examples/discord/init.lua --grants examples/discord/grants.toml
agentctl call discord.set_token -d token='<your-bot-token>' --result-only
```

```bash [cargo]
cargo run -p daemon -- --init examples/discord/init.lua --grants examples/discord/grants.toml
agentctl call discord.set_token -d token='<your-bot-token>' --result-only
```

The token is stored in the OS keyring under the key `discord_token`. You only need to set it once; it persists across restarts.

## Verify

```bash
agentctl services ls
```

You should see both `discord_gateway` and `discord_handler` with state `running`. Then mention your bot in a Discord channel — it should reply within a few seconds.

```bash
agentctl trace -n 20 --follow
```

This streams the trace log so you can watch gateway events, runner calls, and REST responses in real time.

## See also

- [Writing services](/v0/writing/services)
- [ctx.memory reference](/v0/reference/ctx/memory)
- [ctx.ws reference](/v0/reference/ctx/websocket)
- [ctx.http reference](/v0/reference/ctx/http)

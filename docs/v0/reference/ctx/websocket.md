# ctx.ws — WebSocket

`ctx.ws` opens outbound WebSocket connections. It is the natural fit for streaming protocols, real-time gateways, and long-lived integrations such as Discord or Slack bots.

**Required permission:** `net:<host>` — same slug as HTTP.

## Signatures

```lua
ctx.ws.connect(url: string, opts?: {
  heartbeat_ms?: integer,
  heartbeat?:    string,
}) -> Conn
```

### Conn methods

```lua
conn:send(text: string)
conn:send_binary(bytes: string)
conn:recv(timeout_ms?: integer) -> Frame | nil
conn:recv_text(timeout_ms?: integer) -> string | nil
conn:each(fn: fun(frame: Frame))
conn:close()
conn:is_closed() -> boolean
conn:url() -> string
```

## Types

### Conn options

| Field | Type | Description |
|---|---|---|
| `heartbeat_ms` | `integer` | Interval in milliseconds between automatic heartbeat sends. |
| `heartbeat` | `string` | Text payload to send as the heartbeat message. |

### Frame

| Field | Type | Description |
|---|---|---|
| `kind` | `"text" \| "binary"` | Frame type. |
| `text` | `string \| nil` | Text content; present when `kind == "text"`. |

### Conn reference

| Method | Returns | Description |
|---|---|---|
| `:send(text)` | — | Send a text frame. |
| `:send_binary(bytes)` | — | Send a binary frame. |
| `:recv(timeout_ms?)` | `Frame \| nil` | Receive the next frame; returns `nil` on timeout or close. |
| `:recv_text(timeout_ms?)` | `string \| nil` | Receive the next frame and return its text; returns `nil` on timeout or close. |
| `:each(fn)` | — | Iterate frames until the connection closes; calls `fn(frame)` for each. |
| `:close()` | — | Send a close frame and shut down the connection. |
| `:is_closed()` | `boolean` | `true` if the connection has been closed. |
| `:url()` | `string` | The URL this connection was opened against. |

## Permission

```toml
[service.discord_handler]
granted = ["net:discord.com", "net:gateway.discord.gg"]
```

## Examples

```lua
-- Connect to a WebSocket echo server and send/recv one message
agentd.action("ws.echo", function(args, ctx)
  local conn = ctx.ws.connect("wss://echo.websocket.org")
  conn:send(args.message)
  local frame = conn:recv(5000)
  conn:close()
  return frame and frame.text or nil
end)
```

```lua
-- Long-running gateway service with a heartbeat
agentd.service("discord_gateway", { restart = "always" }, function(ctx)
  local token = ctx.secret.get("discord_token")
  local conn = ctx.ws.connect("wss://gateway.discord.gg/?v=10&encoding=json", {
    heartbeat_ms = 41250,
    heartbeat    = json.encode({ op = 1, d = json.null }),
  })

  -- Identify
  conn:send(json.encode({
    op = 2,
    d  = { token = token, intents = 513, properties = { os = "linux" } },
  }))

  conn:each(function(frame)
    if frame.kind ~= "text" then return end
    local msg = json.decode(frame.text)
    ctx.log.debug("op=" .. tostring(msg.op))
    -- dispatch to handler ...
  end)
end)
```

::: tip Heartbeat
Use `heartbeat_ms` and `heartbeat` to keep connections alive automatically without writing your own timer loop.
:::

## See also

- [ctx.http](/v0/reference/ctx/http)
- [Concepts: services](/v0/concepts/services)
- [Recipes: discord-bot](/v0/recipes/discord-bot)
- [Security: permission slugs](/v0/security/permission-slugs)

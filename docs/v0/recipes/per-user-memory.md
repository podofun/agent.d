# Recipe: Per-user memory

Store a rolling history of interactions per user (or per channel, per session) using `ctx.memory`. The history survives daemon restarts and hot reloads, unlike `ctx.state` which is ephemeral. This pattern is lifted directly from the Discord example's channel-history helpers.

## How it works

`ctx.memory.create(namespace)` returns a handle to a durable key/value store that survives daemon restarts. You pick a namespace string that encodes the identity dimension you care about — by caller user, by session, by channel, or any combination:

```lua
-- Keyed by user
ctx.memory.create("history/user/" .. ctx.caller.user)

-- Keyed by session
ctx.memory.create("history/session/" .. ctx.caller.session)

-- Keyed by an application-level ID (e.g. Discord channel)
ctx.memory.create("discord/chan/" .. channel_id)
```

Values are JSON-serialized automatically. A single `"log"` key holds the rolling array.

**Required permissions:** `memory.read:<ns-glob>`, `memory.write:<ns-glob>`

## The rolling-window helpers

```lua
local MAX_TURNS = 20

-- Get or create the memory handle for this user.
local function user_mem(ctx)
  local user = ctx.caller.user or ctx.caller.session or "anon"
  return ctx.memory.create("history/user/" .. user)
end

-- Read the history array (empty table if nothing stored yet).
local function get_history(ctx)
  return user_mem(ctx):get("log") or {}
end

-- Append an entry and trim to MAX_TURNS.
local function push_history(ctx, entry)
  local mem = user_mem(ctx)
  local h   = mem:get("log") or {}
  h[#h + 1] = entry
  while #h > MAX_TURNS do
    table.remove(h, 1)
  end
  mem:set("log", h)
end
```

Each `entry` is any JSON-serializable table, for example:

```lua
{ role = "user",      content = "Hello!" }
{ role = "assistant", content = "Hi there." }
```

## Full example

```lua
-- tools/chat.lua
agentd.tool({
  name = "chat",
  requires = {
    "memory.read:history/**",
    "memory.write:history/**",
    "ai:anthropic",
  },
})

local MAX_TURNS = 20

local function user_mem(ctx)
  local user = ctx.caller.user or ctx.caller.session or "anon"
  return ctx.memory.create("history/user/" .. user)
end

local function get_history(ctx)
  return user_mem(ctx):get("log") or {}
end

local function push_history(ctx, entry)
  local mem = user_mem(ctx)
  local h   = mem:get("log") or {}
  h[#h + 1] = entry
  while #h > MAX_TURNS do
    table.remove(h, 1)
  end
  mem:set("log", h)
end

agentd.action({
  name = "chat.send",
  requires = {
    "memory.read:history/**",
    "memory.write:history/**",
    "ai:anthropic",
  },
  handler = function(args, ctx)
    assert(type(args.message) == "string" and args.message ~= "", "message is required")

    -- Record the incoming message before calling the model.
    push_history(ctx, { role = "user", content = args.message })

    -- Build a prompt from the rolling history.
    local h = get_history(ctx)
    local lines = {}
    for _, e in ipairs(h) do
      lines[#lines + 1] = e.role .. ": " .. e.content
    end
    local prompt = table.concat(lines, "\n")

    -- Ask the model.
    local reply = ctx.ai.ask(prompt, {
      system = "You are a helpful assistant. Continue the conversation naturally.",
    })

    -- Record the assistant's reply.
    push_history(ctx, { role = "assistant", content = reply })

    return { reply = reply }
  end,
})

-- Clear a user's history on demand.
agentd.action({
  name = "chat.clear",
  requires = { "memory.write:history/**" },
  handler = function(args, ctx)
    user_mem(ctx):delete("log")
    return { ok = true }
  end,
})
```

## Entry point

```lua
-- init.lua
import("tools/chat.lua")
```

## grants.toml

```toml
[tool.chat]
granted = [
  "memory.read:history/**",
  "memory.write:history/**",
  "ai:anthropic",
]
```

The `memory.read/write` slugs use glob syntax on the namespace. `history/**` covers every namespace under `history/`, so `history/user/alice`, `history/user/bob`, etc. all match.

## How to run

```bash [release]
agentd --init init.lua --grants grants.toml
```

```bash [cargo]
cargo run -p daemon -- --init init.lua --grants grants.toml
```

## Invoke from the terminal

```bash
# Send a message as user "alice"
agentctl call chat.send \
  --json '{"message": "What is the capital of France?", "user": "alice"}' \
  --result-only
```

::: info Passing user identity
`agentctl call` sends requests over `/ws`. The `user` field in `ctx.caller` is populated from the `user` param in the WebSocket envelope. With `agentctl`, you cannot set `user` directly from the CLI — the caller is the CLI's session. In production, the calling system (a bot, an interface) sets `user` in the `actions.call` params. See the [protocol reference](/v0/reference/protocol).
:::

## Verify

1. Send two messages as the same user:

   ```bash
   agentctl call chat.send -d message="My name is Alice." --result-only
   agentctl call chat.send -d message="What is my name?" --result-only
   ```

   The second reply should reference the name from the first message, confirming the history is being read back.

2. Restart the daemon and repeat the second call — the history must persist, confirming `ctx.memory` is durable.

3. Call `chat.clear` and verify the next response has no memory of earlier messages:

   ```bash
   agentctl call chat.clear --result-only
   agentctl call chat.send -d message="What is my name?" --result-only
   ```

## Adapting to other key dimensions

| Dimension | Namespace pattern |
|---|---|
| Per-user | `"history/user/" .. ctx.caller.user` |
| Per-session | `"history/session/" .. ctx.caller.session` |
| Per-channel (Discord, Slack) | `"discord/chan/" .. channel_id` |
| Per-runner execution | `"runs/" .. ctx.caller.execution` |

You can also store data other than conversation turns — preferences, state machines, counters — in different keys under the same namespace.

## See also

- [ctx.memory reference](/v0/reference/ctx/memory)
- [ctx.caller reference](/v0/reference/ctx/caller)
- [Concepts: memory and state](/v0/concepts/memory-and-state)
- [Discord bot recipe](/v0/recipes/discord-bot)

# The ctx Handle

`ctx` is the handle the runtime passes to every handler, and it is how your Lua code reaches the outside world: programs, files, the network, secrets, memory and models. Each `ctx` call is checked against your grants in `grants.toml` when it happens. If the tool lacks the grant, agent.d asks before the call goes ahead, as described in [Interactive approvals](/v0/security/approvals).

## Where ctx appears

Actions receive `ctx` as their second argument, and services as their only argument:

```lua
-- action handler: ctx is the second argument
agentd.action({
  name    = "git.status",
  requires = { "shell.exec:git" },
  handler = function(args, ctx)
    return { output = ctx.shell("git", { "status" }).stdout }
  end,
})

-- service body: ctx is the first (and only) argument
agentd.service("my_service", function(ctx)
  ctx.log.info("service started")
  -- ...
end)
```

Each call gets its own `ctx`, tied to the caller and the capabilities it is allowed. A service that passes its `ctx` to an action handler does not pass on its grants: the handler keeps the grants of its own call.

## Capability namespaces

| Namespace | Required permission | Reference page |
|---|---|---|
| `ctx.log.{trace,debug,info,warn,error}` | none | [Logging](/v0/reference/ctx/logging) |
| `ctx.shell(bin, args?, opts?)` | `shell.exec[:<bin>]` | [Shell](/v0/reference/ctx/shell) |
| `ctx.fs.{read,write,append,exists,stat,list_dir,remove}` | `fs.read:<path>` / `fs.write:<path>` | [Filesystem](/v0/reference/ctx/fs) |
| `ctx.http.{get,post,request,client}` | `net:<host>` | [HTTP](/v0/reference/ctx/http) |
| `ctx.ws.connect` | `net:<host>` | [WebSocket](/v0/reference/ctx/websocket) |
| `ctx.secret.{get,set,delete,exists,list}` | `secret:<key>` | [Secrets](/v0/reference/ctx/secrets) |
| `ctx.memory.create(ns)` | `memory.read:<ns>` / `memory.write:<ns>` | [Memory](/v0/reference/ctx/memory) |
| `ctx.state.{get,set,delete,keys,clear}` | none | [Memory](/v0/reference/ctx/memory) |
| `ctx.ai.{ask,complete,providers}` | `ai:<provider>` | [AI](/v0/reference/ctx/ai) |
| `ctx.call(name, args?)` | action's own `requires` | [Calls](/v0/reference/ctx/calls) |
| `ctx.run(name, prompt)` | action's own `requires` | [Calls](/v0/reference/ctx/calls) |
| `ctx.structured(name, opts)` | action's own `requires` | [Calls](/v0/reference/ctx/calls) |
| `ctx.tools()` | none | [Calls](/v0/reference/ctx/calls) |
| `ctx.validate_output(value)` | none | [Calls](/v0/reference/ctx/calls) |
| `ctx.caller` | none (read-only) | [Caller](/v0/reference/ctx/caller) |

### Logging

No permission required. Write messages to the trace log at the level that fits:

```lua
ctx.log.info("handling request")
ctx.log.warn("retrying after error: " .. err)
```

### Shell

Permission: `shell.exec`, or `shell.exec:<bin>` for one program.

```lua
-- permission: shell.exec:git
local res = ctx.shell("git", { "status", "--porcelain=v1" })
-- res: { stdout, stderr, exit_code }
```

### Filesystem

Permission: `fs.read:<glob>` for reads, `fs.write:<glob>` for writes.

```lua
-- permission: fs.read:/tmp/**
local content = ctx.fs.read("/tmp/output.txt")

-- permission: fs.write:/tmp/**
ctx.fs.write("/tmp/result.txt", content)
```

### HTTP

Permission: `net:<host>` for each host contacted.

```lua
-- permission: net:api.example.com
local resp = ctx.http.get("https://api.example.com/data")
local data = resp:json()
```

### WebSocket

Permission: `net:<host>`.

```lua
-- permission: net:gateway.example.com
local conn = ctx.ws.connect("wss://gateway.example.com/ws")
conn:each(function(frame)
  if frame.kind == "text" then
    -- process frame.text
  end
end)
```

### Secrets

Permission: `secret:<key>` or `secret:*`.

```lua
-- permission: secret:discord_token
local token = ctx.secret.get("discord_token")
```

### Memory

Durable memory, which survives restarts, needs `memory.read:<ns>` or `memory.write:<ns>`. Short-lived state needs no permission.

```lua
-- permission: memory.read:discord/**, memory.write:discord/**
local mem = ctx.memory.create("discord/chan/" .. channel_id)
local history = mem:get("log") or {}
mem:set("log", history)

-- ephemeral — no permission needed
ctx.state.set("bot_user_id", id)
local id = ctx.state.get("bot_user_id")
```

### AI

Permission: `ai:<provider>`.

```lua
-- permission: ai:anthropic
local reply = ctx.ai.ask("Summarise this diff.", {
  provider   = "anthropic",
  model      = "claude-opus-4-7",
  max_tokens = 512,
})
```

### Cross-component calls

`ctx.call` and `ctx.run` call registered actions and runners. The called action's own `requires` still apply, and so does every other permission check. `ctx.call` cannot call `confirm` actions.

```lua
-- no extra permission on the caller; inner action's requires apply
local result = ctx.call("git.status", { cwd = "/repo" })
local out    = ctx.run("backend_reviewer", "Review the staged diff.")
```

### Caller identity

`ctx.caller` is a read-only table set by the runtime. No permission required.

```lua
-- fields present depending on caller type:
-- { interface?, runner?, service?, session?, user?, execution? }
ctx.log.info("called by session " .. (ctx.caller.session or "?"))
```

## Helper globals

These functions are available everywhere in Lua, outside `ctx`. They need no permission.

| Global | Description | Reference |
|---|---|---|
| `async(fn)` / `await(handle)` | Spawn and join coroutines | [Concurrency](/v0/reference/ctx/concurrency) |
| `parallel(fns, opts?)` | Fan-out, collect results in order | [Concurrency](/v0/reference/ctx/concurrency) |
| `parallel_map(items, fn, opts?)` | Map over a list concurrently | [Concurrency](/v0/reference/ctx/concurrency) |
| `channel(name?)` | Create or retrieve a named channel | [Concurrency](/v0/reference/ctx/concurrency) |
| `timer.after(ms, fn)` / `timer.every(ms, fn)` | One-shot and recurring timers | [Concurrency](/v0/reference/ctx/concurrency) |
| `sleep(ms)` | Yield for a duration | [Concurrency](/v0/reference/ctx/concurrency) |
| `json.encode` / `json.decode` / `json.null` / `json.is_null` | JSON serialisation | [Stdlib](/v0/reference/ctx/stdlib) |
| `string.trim`, `string.split`, `string.contains`, … | String helpers | [Stdlib](/v0/reference/ctx/stdlib) |
| `import(path)` | Load a Lua file relative to `init.lua` | [init.lua](/v0/writing/init) |

## See also

- [Permissions concept](/v0/concepts/permissions)
- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)
- [Concurrency reference](/v0/reference/ctx/concurrency)

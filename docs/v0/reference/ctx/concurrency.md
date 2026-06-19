# Concurrency

agent.d exposes a set of global functions for cooperative concurrency: coroutine spawning, parallel fan-out, channels, and timers. These are **globals** — they are available everywhere, not under `ctx.*`.

**Required permission:** none.

## Signatures

```lua
sleep(ms: integer)

async(fn: fun(): T?) -> userdata
await(handle: userdata) -> T

parallel(fns: (fun(): any)[], opts?: { limit?: integer, settled?: boolean }) -> any[]
parallel_map(items: T[], fn: fun(item: T, index: integer): R, opts?: { limit?: integer, settled?: boolean }) -> R[]

channel(name?: string) -> Channel
-- Channel:
channel:send(msg: any)
channel:recv() -> any
channel:try_recv() -> any | nil
channel:close()
channel:is_closed() -> boolean
channel.name -> string | nil

timer.after(ms: integer, fn: fun()) -> Timer
timer.every(ms: integer, fn: fun()) -> Timer
-- Timer:
timer:stop()
```

## Functions

### `sleep(ms)`

Suspend the current coroutine for `ms` milliseconds without blocking other coroutines.

### `async(fn)` / `await(handle)`

`async(fn)` spawns `fn` as a coroutine and returns an opaque handle. `await(handle)` blocks the caller until the coroutine completes and returns its return value.

```lua
local handle = async(function()
  sleep(1000)
  return "done"
end)
local result = await(handle)  -- "done"
```

### `parallel(fns, opts?)`

Run multiple functions concurrently and return their results in the same order as `fns`.

| Option | Type | Description |
|---|---|---|
| `limit` | `integer` | Maximum number of concurrent coroutines. No limit if omitted. |
| `settled` | `boolean` | When `true`, collect all results even if some functions error; errors are returned in-place. When `false` (default), the first error propagates immediately. |

### `parallel_map(items, fn, opts?)`

Like `parallel`, but maps a function over an array. `fn` receives each `(item, index)` pair. Accepts the same `limit` and `settled` options.

### `channel(name?)`

Create an unbuffered channel for passing values between coroutines. Pass a `name` to get-or-create a named process-wide channel (useful for communication between services); omit it for an anonymous channel local to the current scope.

| Method | Description |
|---|---|
| `:send(msg)` | Send a value; blocks until a receiver is ready. |
| `:recv()` | Receive a value; blocks until a sender is ready. |
| `:try_recv()` | Non-blocking receive; returns `nil` if no message is available. |
| `:close()` | Close the channel; subsequent sends raise an error, recvs drain then return `nil`. |
| `:is_closed()` | Return `true` if the channel has been closed. |
| `.name` | The channel's name, or `nil` for anonymous channels. |

### `timer.after(ms, fn)` / `timer.every(ms, fn)`

Schedule a one-shot or repeating callback. Both return a `Timer` with a single `:stop()` method that cancels future firings.

## Examples

```lua
-- Fan out three HTTP requests in parallel
agentd.action("news.fetch", function(args, ctx)
  local urls = { "https://a.example.com", "https://b.example.com", "https://c.example.com" }
  local results = parallel_map(urls, function(url)
    return ctx.http.get(url).body
  end, { limit = 3 })
  return results
end)
```

```lua
-- Producer/consumer via a named channel
agentd.service("producer", function(ctx)
  local ch = channel("work_queue")
  local i = 0
  timer.every(2000, function()
    i = i + 1
    ch:send({ id = i, task = "ping" })
  end)
  -- run forever
  while true do sleep(60000) end
end)

agentd.service("consumer", function(ctx)
  local ch = channel("work_queue")
  while true do
    local item = ch:recv()
    ctx.log.info("processing task id=" .. item.id)
  end
end)
```

```lua
-- One-shot timer to trigger cleanup after a delay
agentd.service("cleanup_scheduler", function(ctx)
  timer.after(30000, function()
    ctx.log.info("running scheduled cleanup")
    ctx.call("cache.clear")
  end)
  while true do sleep(60000) end
end)
```

```lua
-- Await two concurrent calls
agentd.action("review.parallel", function(args, ctx)
  local h1 = async(function()
    return ctx.run("backend_reviewer", args.prompt)
  end)
  local h2 = async(function()
    return ctx.run("security_reviewer", args.prompt)
  end)
  local backend  = await(h1)
  local security = await(h2)
  return { backend = backend.text, security = security.text }
end)
```

::: tip limit option
Use `limit` in `parallel` and `parallel_map` to avoid overwhelming downstream services when mapping over large arrays.
:::

## See also

- [Concepts: runtime](/v0/concepts/runtime)
- [Concepts: services](/v0/concepts/services)
- [ctx.call — running runners](/v0/reference/ctx/calls)
- [ctx — overview](/v0/reference/ctx/)

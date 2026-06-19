# ctx.log — Logging

`ctx.log` writes structured messages to the agent.d trace/log sink. No permission is required; all handlers and services can use it freely.

## Signatures

```lua
ctx.log.trace(msg: string)
ctx.log.debug(msg: string)
ctx.log.info(msg: string)
ctx.log.warn(msg: string)
ctx.log.error(msg: string)
```

## Methods

| Method | Level | When to use |
|---|---|---|
| `ctx.log.trace(msg)` | TRACE | Very fine-grained diagnostics; high volume. |
| `ctx.log.debug(msg)` | DEBUG | Development-time detail; suppressed in production filters. |
| `ctx.log.info(msg)` | INFO | Normal operational events. |
| `ctx.log.warn(msg)` | WARN | Recoverable anomalies or deprecated usage. |
| `ctx.log.error(msg)` | ERROR | Failures that require attention. |

**Required permission:** none.

All output goes to the trace sink (`--trace-file`, default `$XDG_STATE_HOME/agentd/trace.jsonl`). Stream it live with `agentctl trace -f`.

## Parameters

| Parameter | Type | Description |
|---|---|---|
| `msg` | `string` | The message to log. |

Log entries carry the action or service name automatically; you do not need to prefix messages with context.

## Examples

```lua
agentd.action("git.status", function(args, ctx)
  ctx.log.info("running git status")

  local result = ctx.shell("git", { "status", "--short" })

  if result.exit_code ~= 0 then
    ctx.log.error("git status failed: " .. result.stderr)
    error("git exited " .. result.exit_code)
  end

  ctx.log.debug("output: " .. result.stdout)
  return result.stdout
end)
```

```lua
agentd.service("poller", function(ctx)
  ctx.log.info("poller starting")
  while true do
    ctx.log.trace("tick")
    sleep(5000)
  end
end)
```

## See also

- [ctx — overview](/v0/reference/ctx/)
- [Operations: observability](/v0/operations/observability)
- [Concurrency](/v0/reference/ctx/concurrency)
- [ctx.shell](/v0/reference/ctx/shell)

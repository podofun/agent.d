# Tools and Actions

Tools are the unit of capability in agent.d. A tool is a named namespace; an action is one callable operation inside it. Together they define what the daemon can do on behalf of a caller.

## Tools

You register a tool with `agentd.tool`. The tool declaration names the namespace and optionally declares the permissions the tool's actions will need:

```lua
agentd.tool({
  name = "git",
  requires = { "shell.exec:git" },
})
```

The `requires` field is a declaration — it tells readers and the package system what permissions this tool needs. It does **not** grant those permissions; only `grants.toml` can do that.

## Actions

An action is a single operation identified by a fully-qualified `tool.action` name (e.g. `git.status`, `git.diff`). You register it with `agentd.action`:

```lua
agentd.action({
  name    = "git.status",
  requires = { "shell.exec:git" },
  handler = function(args, ctx)
    local res = ctx.shell("git", { "-C", args.cwd or ".", "status", "--porcelain=v1" })
    return { status = res.stdout, exit_code = res.exit_code }
  end,
})
```

There is also a short form for simple inline actions:

```lua
agentd.action("tool.action", function(args, ctx)
  -- ...
end)
```

### Fields

| Field | Required | Meaning |
|---|---|---|
| `name` | yes | Fully-qualified `tool.action` identifier. |
| `handler` | yes | `function(args, ctx)` called when the action is invoked. |
| `requires` | no | Permission slugs the handler needs (e.g. `"shell.exec:git"`). |
| `confirm` | no | When `true`, each call requires interactive operator approval unless `auto_confirm` covers it. |
| `tool` | no | Owning tool name; inferred from the `name` prefix when omitted. |

## The `ctx` handle

The second argument to every handler is `ctx` — a per-invocation capability handle. It exposes only the capabilities the permission engine approved for this call:

```lua
handler = function(args, ctx)
  ctx.log.info("running git.status")
  local res = ctx.shell("git", { "status" })
  return { output = res.stdout }
end
```

`ctx` gives you shell, filesystem, HTTP, WebSocket, secrets, memory, AI calls, and inter-component calls — each gated by the corresponding permission slug.

::: info
The full `ctx` API is documented in the [ctx reference](/v0/reference/ctx/). The `shell` namespace is at [ctx.shell](/v0/reference/ctx/shell).
:::

## Calling actions

Clients call actions over the WebSocket data plane. From the console:

```bash [release]
agentctl call git.status
agentctl call git.diff -d staged=true
```

```bash [cargo]
cargo run -p agentd-cli -- call git.status
```

From another action, use `ctx.call`:

```lua
local result = ctx.call("git.status", { cwd = "/workspace" })
```

`ctx.call` checks the inner action's `requires` against the current caller's grants before executing.

## See also

- [Writing tools](/v0/writing/tools)
- [ctx reference](/v0/reference/ctx/)
- [Permissions](/v0/concepts/permissions)
- [Tutorial: first tool](/v0/tutorial/first-tool)

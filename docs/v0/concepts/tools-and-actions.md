# Tools and Actions

Tools and actions are how you give agent.d things to do. A tool is a named group, such as `git`. An action is one operation inside it that a client, a runner or another action can call, such as `git.status`.

## Tools

You register a tool with `agentd.tool`. The declaration names the tool and can list the permissions its actions need:

```lua
agentd.tool({
  name = "git",
  requires = { "shell.exec:git" },
})
```

The `requires` field tells readers and the package system what permissions this tool needs. The permissions themselves are granted in `grants.toml`.

## Actions

An action is a single operation with a full `tool.action` name, for example `git.status` or `git.diff`. You register it with `agentd.action`:

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
| `name` | yes | Full `tool.action` name. |
| `handler` | yes | `function(args, ctx)` that runs when the action is called. |
| `requires` | no | Permission slugs the handler needs (for example `"shell.exec:git"`). |
| `confirm` | no | When `true`, each call needs an approval, unless `[approval].skip` in `grants.toml` lists the action. |
| `tool` | no | The tool the action belongs to. When omitted, it is taken from the `name` prefix. |
| `input` | no | Short-form schema for the action's arguments. Runners show it to the model as the tool's `input_schema`, and arguments are checked against it before the handler runs. |
| `output` | no | Schema for the return value, checked after the handler runs. The model does not see it. |
| `strict` | no | Default `true`. Adds `additionalProperties: false` to the compiled `input`, so a call with extra keys fails. |

When an action declares `input`, the model fills in the declared fields, and bad arguments are rejected before the handler runs. See [Writing tools](/v0/writing/tools#describing-arguments-and-results) for the schema syntax, its shortcuts and its current limits.

## The `ctx` handle

The second argument to every handler is `ctx`, a handle created for each call. It offers the capabilities that were approved for this call:

```lua
handler = function(args, ctx)
  ctx.log.info("running git.status")
  local res = ctx.shell("git", { "status" })
  return { output = res.stdout }
end
```

`ctx` gives you shell, filesystem, HTTP, WebSocket, secrets, memory, AI calls, and calls to other actions. Each needs its own permission slug.

::: info
The full `ctx` API is documented in the [ctx reference](/v0/reference/ctx/). The `shell` namespace is at [ctx.shell](/v0/reference/ctx/shell).
:::

## Calling actions

Clients call actions over the `/ws` WebSocket. From the terminal:

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

Before it runs the inner action, `ctx.call` checks that action's `requires` against the current caller's grants.

## See also

- [Writing tools](/v0/writing/tools)
- [ctx reference](/v0/reference/ctx/)
- [Permissions](/v0/concepts/permissions)
- [Tutorial: first tool](/v0/tutorial/first-tool)

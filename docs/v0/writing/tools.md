# Writing Tools and Actions

A **tool** is a named namespace; **actions** are the callable operations inside
it. This page covers `agentd.tool` and `agentd.action`, the handler signature,
permission declarations, and the `confirm` flag — using the `git` tool as the
worked example.

## Registering a tool

```lua
agentd.tool({
  name    = "git",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
})
```

`requires` on a tool **declares** the permissions the tool's actions need as a
group. Declaring does not grant — grants live in
[grants.toml](/v0/security/grants). You can also declare `requires` on each action
individually (or on both; the union is checked).

## Registering actions

### Full form

```lua
agentd.action({
  name    = "git.diff",                  -- fully-qualified "tool.action"
  requires = { "shell.exec:git" },       -- permission: shell.exec:git
  confirm = false,                       -- set true to require interactive approval
  tool    = "git",                       -- optional: inferred from name if omitted
  handler = function(args, ctx)
    -- args: table of caller-supplied arguments
    -- ctx:  per-invocation capability handle
    local res = ctx.shell("git", { "diff" })
    return { diff = res.stdout, exit_code = res.exit_code }
  end,
})
```

### Short form

When an action needs no metadata beyond a name and handler, use the two-argument
shorthand:

```lua
agentd.action("git.ping", function(args, ctx)
  return { ok = true }
end)
```

## The handler signature

```lua
function(args, ctx) -> JSON-serializable table | nil
```

| Parameter | Type | Description |
|---|---|---|
| `args` | `table \| nil` | Caller-supplied arguments. Always check for `nil` before indexing. |
| `ctx` | `table` | Per-invocation capability handle. See [ctx overview](/v0/writing/context). |

Return value must be a JSON-serializable Lua table (or `nil`). Returning a
non-serializable value (e.g. a function or userdata) is a runtime error.

## Permission slugs in `requires`

List every permission slug the handler will use at runtime. The runtime
intersects these declared needs with the grants from `grants.toml` — if a
required permission is not granted, the action is denied before the handler
runs.

Common slugs:

| Slug | What it gates |
|---|---|
| `shell.exec:git` | Running the `git` binary via `ctx.shell` |
| `net:api.example.com` | HTTP/WebSocket to that host via `ctx.http` / `ctx.ws` |
| `fs.read:/tmp/**` | Reading paths under `/tmp` via `ctx.fs` |
| `fs.write:/tmp/**` | Writing paths under `/tmp` via `ctx.fs` |
| `secret:my_key` | Keyring access via `ctx.secret` |
| `memory.read:ns/**` | Reading a durable memory namespace via `ctx.memory` |
| `memory.write:ns/**` | Writing a durable memory namespace via `ctx.memory` |
| `ai:anthropic` | Model calls via `ctx.ai` |

See [Permission slugs](/v0/security/permission-slugs) for the full reference.

## `confirm = true` — interactive approval

```lua
agentd.action({
  name    = "git.push",
  requires = { "shell.exec:git" },
  confirm = true,    -- every call is held for operator approval
  handler = function(args, ctx)
    return { exit_code = ctx.shell("git", { "push" }).exit_code }
  end,
})
```

When `confirm = true`, every invocation is sent to the approval plane and held
until an operator resolves it (`allow_once`, `allow_forever`, or `deny`). If no
operator is connected, the request times out and fails closed.

`ctx.call()` cannot invoke `confirm`-gated actions — only direct callers (runners,
interfaces, services) can trigger them.

[`[policy].auto_confirm`](/v0/security/grants) pre-approves specific action names so
they pass the `confirm` gate automatically.

## Worked example — the git tool

The full `examples/tools/git.lua` shows the pattern end to end:

```lua
agentd.tool({
  name     = "git",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
})

local function git(ctx, args, sub)
  args = args or {}
  local argv = { "-C", args.cwd or "." }
  for _, a in ipairs(sub) do
    table.insert(argv, a)
  end
  local res = ctx.shell("git", argv, { separate_stderr = false })
  return { exit_code = res.exit_code, output = res.stdout }
end

agentd.action({
  name     = "git.diff",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
  handler  = function(args, ctx)
    args = args or {}
    local sub = { "diff" }
    if args.staged then
      table.insert(sub, "--staged")
    end
    ctx.log.info("git.diff cwd=" .. (args.cwd or "."))
    local r = git(ctx, args, sub)
    return { diff = r.output, exit_code = r.exit_code }
  end,
})

agentd.action({
  name     = "git.status",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
  handler  = function(args, ctx)
    local r = git(ctx, args, { "status", "--porcelain=v1" })
    return { status = r.output, exit_code = r.exit_code }
  end,
})
```

The corresponding `grants.toml` entry that actually enables the permission:

```toml
[tool.git]
granted = ["shell.exec:git"]
```

## See also

- [Tools and actions concept](/v0/concepts/tools-and-actions)
- [ctx overview](/v0/writing/context)
- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)

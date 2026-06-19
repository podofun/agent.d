# Step 2 — Write Your First Tool

A **tool** is a namespace; an **action** is the callable unit inside it. This page shows you how to register a `git` tool with two actions that shell out to the `git` binary through `ctx.shell`.

## What you are building

`tools/git.lua` will expose:

| Action | What it does |
|--------|-------------|
| `git.status` | Runs `git status --porcelain=v1` and returns the output. |
| `git.diff` | Runs `git diff` (or `git diff --staged` when `args.staged` is true). |

## Register the tool namespace

Every action belongs to a tool. Declare the tool first — this is also where you advertise the permissions the tool's actions will need:

```lua
agentd.tool({
    name = "git",
    requires = { "shell.exec:git" },
})
```

`requires` **declares** intent but never self-grants. The daemon's permission engine checks `requires` at call time; the actual grant comes from `grants.toml` in [Step 3](/v0/tutorial/permissions).

## A shared helper

Before writing the actions, add a small private function so both actions share the same logic for building argv and calling `ctx.shell`:

```lua
local function git(ctx, args, sub)
    args = args or {}
    local argv = { "-C", args.cwd or "." }
    for _, a in ipairs(sub) do
        table.insert(argv, a)
    end
    local res = ctx.shell("git", argv, { separate_stderr = false })
    return { exit_code = res.exit_code, output = res.stdout }
end
```

### Understanding `ctx.shell`

`ctx.shell(bin, args, opts)` runs a process. It takes:

- `bin` — the bare binary name (no shell, no PATH expansion beyond normal lookup).
- `args` — an array of string arguments.
- `opts` — optional table: `cwd`, `stdin`, `separate_stderr` (default `true`).

It returns `{ stdout, stderr, exit_code }`. When `separate_stderr = false`, stderr is merged into `stdout`.

Permission required: `shell.exec:git` — the specifier is the bare binary name, matching what you pass as `bin`.

## Register `git.diff`

```lua
agentd.action({
    name = "git.diff",
    requires = { "shell.exec:git" },
    handler = function(args, ctx)
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
```

### The `handler(args, ctx)` signature

Every action handler receives two arguments:

- `args` — a table of caller-supplied parameters (from `agentctl call`, the WebSocket API, or another action calling `ctx.call`).
- `ctx` — the per-invocation capability handle. All I/O goes through `ctx`: `ctx.shell`, `ctx.fs`, `ctx.http`, `ctx.log`, and so on.

The return value is JSON-serialized and sent back to the caller as `result`.

`ctx.log.info(msg)` emits a structured log line at the given level. Available levels: `trace`, `debug`, `info`, `warn`, `error`. No permission required.

## Register `git.status`

```lua
agentd.action({
    name = "git.status",
    requires = { "shell.exec:git" },
    handler = function(args, ctx)
        local r = git(ctx, args, { "status", "--porcelain=v1" })
        return { status = r.output, exit_code = r.exit_code }
    end,
})
```

## The complete `tools/git.lua`

```lua
agentd.tool({
    name = "git",
    requires = { "shell.exec:git" },
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
    name = "git.diff",
    requires = { "shell.exec:git" },
    handler = function(args, ctx)
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
    name = "git.status",
    requires = { "shell.exec:git" },
    handler = function(args, ctx)
        local r = git(ctx, args, { "status", "--porcelain=v1" })
        return { status = r.output, exit_code = r.exit_code }
    end,
})
```

::: warning `requires` does not grant
Writing `requires = { "shell.exec:git" }` tells the engine what the action needs. Without a matching entry in `grants.toml`, every call is denied. You will fix that in the next step.
:::

## Next step

[Step 3 — Permissions →](/v0/tutorial/permissions)

## See also

- [Concepts: tools and actions](/v0/concepts/tools-and-actions)
- [ctx.shell reference](/v0/reference/ctx/shell)
- [ctx.log reference](/v0/reference/ctx/logging)
- [Writing tools](/v0/writing/tools)

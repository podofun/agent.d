# Shell sandbox

agent.d's shell execution model is designed to limit what a tool action can do even when `shell.exec` is granted. This page describes the sandbox boundaries you can rely on.

## argv-only invocation

`ctx.shell` takes a binary name and an explicit argument list — there is no shell string interpolation:

```lua
-- Safe: arguments are passed directly to the process
local out = ctx.shell("git", { "diff", "--stat", args.path })

-- The structured form is equivalent
local out = ctx.shell({ bin = "git", args = { "diff", "--stat", args.path } })
```

Because the runtime calls the binary directly (argv-only), there is **no shell** interpreting the arguments. Shell metacharacters (`|`, `;`, `$()`, backticks, redirects) in user-supplied values cannot trigger shell injection — they are passed as literal strings to the child process.

::: warning
This protection applies only to the shell invocation itself. If you pass user input to a program that *itself* interprets it as a script (e.g. passing arbitrary strings to `bash -c`), injection is still possible. Validate inputs before passing them to such programs.
:::

## Network namespace sandbox (Linux)

On Linux, child processes spawned via `ctx.shell` are confined inside a network namespace sandbox. This prevents a child process from making arbitrary outbound network connections that would bypass the `net:*` permission checks applied at the Lua API layer.

::: info
Network namespace confinement is a Linux-specific feature. Do not rely on equivalent network isolation on other platforms.
:::

## Fail-closed

The sandbox is designed to fail closed. If confinement cannot be established, the call fails rather than proceeding without isolation. This means a sandbox configuration or OS environment that does not support the required isolation will cause `ctx.shell` calls to error rather than silently running unconfined.

## What the sandbox does not cover

- **Filesystem access**: Child processes inherit the daemon's filesystem view. Use `fs.read` / `fs.write` grants and, where possible, narrow the `cwd` option to limit effective file access.
- **CPU and memory**: There are no resource limits on child processes at this time.
- **Specifier matching**: The `shell.exec:<bin>` specifier is matched against the first argument to `ctx.shell`. Use specific specifiers (`shell.exec:git`) rather than the bare `shell.exec` to limit which binaries can be invoked.

## See also

- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)
- [Best practices](/v0/security/best-practices)
- [`ctx.shell` reference](/v0/reference/ctx/shell)

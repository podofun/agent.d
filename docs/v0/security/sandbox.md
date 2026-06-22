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

## Filesystem confinement

A child process spawned by `ctx.shell` can only write to the paths you grant with `fs.write`. A write anywhere else fails, even with `shell.exec` granted — so a tool cannot modify files outside what its grants allow. Enforced on Linux, macOS, and Windows.

Keep grants narrow, and use the `cwd` option to scope where a tool's relative paths resolve.

## Network confinement

A child process can only reach hosts you allow with `net:` grants. With no `net:` grant it has no outbound network, and it cannot bypass the grants by connecting directly — the hosts allowed at the Lua API layer are the only ones a spawned binary can reach. Enforced on Linux, macOS, and Windows.

### Windows: one-time network setup

On Windows, sandboxed networking needs a one-time setup that requires Administrator. Run it once, in an elevated terminal:

```powershell
daemon --install-sandbox
```

It prints a confirmation and exits. The daemon itself then runs normally, without Administrator — you only do this once per machine.

Until you run it, `ctx.shell` calls that need network fail closed with a message pointing you here. Calls that don't use the network are unaffected.

## Fail-closed

The sandbox fails closed: if confinement cannot be established, the call errors rather than running unconfined.

## What the sandbox does not cover

- **CPU and memory**: there are no resource limits on child processes at this time.
- **Specifier matching**: the `shell.exec:<bin>` specifier is matched against the first argument to `ctx.shell`. Prefer specific specifiers (`shell.exec:git`) over the bare `shell.exec` to limit which binaries can run.

## See also

- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)
- [Best practices](/v0/security/best-practices)
- [`ctx.shell` reference](/v0/reference/ctx/shell)

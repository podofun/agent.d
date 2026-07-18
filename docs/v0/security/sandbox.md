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

A child process spawned by `ctx.shell` can only write to the paths you grant with `fs.write`, and read only its `fs.read` grants plus its `cwd`. Access anywhere else fails, even with `shell.exec` granted — so a tool cannot read or modify files outside what its grants allow. Enforced on Linux, macOS, and Windows.

Keep grants narrow, and use the `cwd` option to scope where a tool's relative paths resolve.

::: warning Windows performance and footprint
Linux (Landlock) and macOS (Seatbelt) confine the filesystem at runtime and change nothing on disk. Windows has no equivalent lightweight primitive, so agent.d confines by temporarily adjusting the ACLs of the granted paths. Two things follow:

- **Granting a large directory can be slow** the first time it's used (Windows applies the grant to every file in the subtree). Keep grants narrow; small grants are instant.
- **The change is transient.** agent.d records its ACL entries and removes them on shutdown, on `agentd --uninstall-sandbox`, and on the next start after a crash.

:::

## Network confinement

A child process can only reach hosts you allow with `net:` grants. With no `net:` grant it has no outbound network, and it cannot bypass the grants by connecting directly — the hosts allowed at the Lua API layer are the only ones a spawned binary can reach. Enforced on Linux, macOS, and Windows.

The grant syntax is identical everywhere: `net:1.2.3.4` (literal IP), `net:api.example.com` (host), `net:api.example.*` (suffix wildcard). You write the same `grants.toml` on every platform.

### One-time network setup (macOS and Windows)

The daemon **never runs elevated**. Sandboxed networking needs a small one-time setup that does — run it once per machine, then the daemon runs unprivileged from then on.

**macOS:**

```bash
sudo agentd --install-sandbox    # sudo agentd --uninstall-sandbox to reverse
```

**Windows** (elevated terminal):

```powershell
agentd --install-sandbox
```

Each prints a confirmation and exits. Until you run it, `ctx.shell` calls that need network fail closed with a message pointing here; calls that don't use the network are unaffected. **Linux needs no setup.**

### Platform notes

On every platform a binary can reach exactly the hosts and IPs your `net:` grants cover — literal IPs, host names, and suffix wildcards — and nothing else, over both IPv4 and IPv6. Two behaviors differ slightly today:

- **macOS wildcards** rely on the destination having correct reverse DNS, which is true for most services. If a wildcard-granted host isn't reachable, grant it by its concrete name or IP instead.
- **Windows** does not yet honor wildcard host grants — a connection to a wildcard-granted host fails closed. Grant those hosts by concrete name or IP on Windows for now.

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

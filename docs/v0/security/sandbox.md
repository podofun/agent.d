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

Every program that `ctx.shell` starts, and every program that it starts in turn, has these file rules:

- **Write:** only inside the directories granted with `fs.write`.
- **Read:** inside the directories granted with `fs.read` or `fs.write`, plus the system files that any program needs to start (binaries, libraries, and system configuration).

Any other read or write fails with a permission error. The `shell.exec` grant does not change these rules. For example, with `fs.write:/srv/app/**`, a tool can edit files under `/srv/app`, but it cannot read `/home/alice/.ssh` or write to `/etc`.

Keep grants narrow, and use the `cwd` option to scope where a tool's relative paths resolve.

## Network confinement

A child process can only reach hosts you allow with `net:` grants. With no `net:` grant it has no outbound network, and it cannot bypass the grants by connecting directly. The hosts allowed at the Lua API layer are the only ones a spawned binary can reach. This covers every protocol (TCP, UDP, and QUIC) over both IPv4 and IPv6.

A `net:` grant names a destination in one of three forms: `net:1.2.3.4` (an IP address), `net:api.example.com` (a host name), or `net:api.example.*` (a host name with a wildcard suffix).

## One-time setup

On macOS and Windows, only the operating system's administrator can change firewall rules. agent.d itself always runs as a normal user, so it installs a small helper service, once per machine, that applies these rules for it. On macOS, the helper is a system service with a few dedicated sandbox user accounts. On Windows, it is a Windows service. Install it with administrator rights:

```bash
agentd --install-sandbox
```

On macOS, prefix it with `sudo`. On Windows, run it from an elevated terminal.

Until you run it, `ctx.shell` calls that need network fail closed with a message pointing here. Calls that don't use the network are unaffected. `agentd --uninstall-sandbox` reverses it. Linux needs no setup.

## Fail-closed

The sandbox fails closed: if confinement cannot be established, the call errors rather than running unconfined. The error names what is missing.

## See also

- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)
- [Best practices](/v0/security/best-practices)
- [`ctx.shell` reference](/v0/reference/ctx/shell)

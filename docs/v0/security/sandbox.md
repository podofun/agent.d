# Shell sandbox

When an action runs a program with `ctx.shell`, that program runs in a sandbox. The sandbox limits which files and hosts the program can reach, even after you grant `shell.exec`. This page describes those limits.

## argv-only invocation

`ctx.shell` takes a program name and a list of arguments. It does not build a shell command string:

```lua
-- Safe: arguments are passed directly to the process
local out = ctx.shell("git", { "diff", "--stat", args.path })

-- The structured form is equivalent
local out = ctx.shell({ bin = "git", args = { "diff", "--stat", args.path } })
```

agent.d starts the program directly, with no shell in between to interpret the arguments. Shell metacharacters (`|`, `;`, `$()`, backticks, redirects) in user-supplied values reach the program as plain text, so they cannot inject shell commands.

::: warning
This protection covers how agent.d starts the program. A program that treats its own input as a script, such as `bash -c`, can still be injected. Validate input before you pass it to such programs.
:::

## Filesystem confinement

Every program that `ctx.shell` starts, and every program that it starts in turn, has these file rules:

- **Write:** only inside the directories granted with `fs.write`.
- **Read:** inside the directories granted with `fs.read` or `fs.write`, plus the system files that any program needs to start (binaries, libraries, and system configuration).

Any other read or write fails with a permission error, whatever `shell.exec` grants. For example, with `fs.write:/srv/app/**`, a tool can edit files under `/srv/app`. Reading `/home/alice/.ssh` or writing to `/etc` fails.

Keep grants narrow, and use the `cwd` option to set the folder that a tool's relative paths start from.

## Network confinement

A program started by `ctx.shell` can reach only the hosts you allow with `net:` grants, the same hosts your Lua code can reach. With no `net:` grant it has no outbound network. Connecting directly to an address does not get around this. The limit covers TCP, UDP and QUIC over both IPv4 and IPv6.

A `net:` grant names a destination in one of three forms: `net:1.2.3.4` (an IP address), `net:api.example.com` (a host name), or `net:api.example.*` (a host name with a wildcard suffix).

## Denials and the sandbox

The sandbox works by opening access. It cannot cut one file out of a folder it opens, or one host out of a pattern it allows. When a shell command could reach something in `[denial]`, agent.d refuses the command before it starts. For example, if you grant `fs.read:/home/me/project/**` and deny `fs.read:/home/me/project/.env`, every shell command with that grant is refused. Keep denied files outside the folders that you grant to shell commands, or grant narrower folders.

See [What a denial guarantees](/v0/security/denials#shell-commands) for every case and the error message.

## One-time setup

On macOS and Windows, only an administrator can change firewall rules. agent.d runs as a normal user, so it uses a small helper service, installed once per machine, that applies these rules for it. On macOS, the helper is a system service with a few dedicated sandbox user accounts. On Windows, it is a Windows service. Install it with administrator rights:

```bash
agentd --install-sandbox
```

On macOS, prefix it with `sudo`. On Windows, run it from an elevated terminal.

Until you install it, `ctx.shell` calls that need the network fail with a message that points to this page. Calls without network access work as usual. `agentd --uninstall-sandbox` removes the helper. Linux needs no setup.

## Fail-closed

The sandbox fails closed. If agent.d cannot set up the sandbox for a call, the call fails with an error that names what is missing, and the program does not start.

## See also

- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)
- [What a denial guarantees](/v0/security/denials)
- [Best practices](/v0/security/best-practices)
- [`ctx.shell` reference](/v0/reference/ctx/shell)

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

The grant syntax is identical everywhere: `net:1.2.3.4` (literal IP), `net:api.example.com` (host), `net:api.example.*` (suffix wildcard). You write the same `grants.toml` on every platform.

### One-time network setup (macOS and Windows)

The daemon **never runs elevated**. Sandboxed networking needs a small one-time setup that does — run it once per machine, then the daemon runs unprivileged forever after.

**macOS** (installs a tiny root helper — `agentd-pf-broker` — as a launchd daemon; the daemon talks to it over a uid-checked socket):

```bash
sudo agentd --install-sandbox    # sudo agentd --uninstall-sandbox to reverse
```

**Windows** (elevated terminal, once):

```powershell
daemon --install-sandbox
```

Each prints a confirmation and exits. Until you run it, `ctx.shell` calls that need network fail closed with a message pointing here; calls that don't use the network are unaffected. **Linux needs no setup** — it uses rootless user + network namespaces set up per call.

### How enforcement works, per platform

All three default-deny outbound (both IPv4 and IPv6) and permit only the IPs backing your `net:` grants. They differ only in mechanism:

| | Enforcement | Names | Wildcards |
|---|---|---|---|
| **Linux** | rootless netns + nftables redirect to an in-daemon relay; DNS pinned at query time | live (query intercepted) | full |
| **macOS** | `pf` redirect (scoped to a broker-leased uid) to an in-daemon relay | live, resolved at connect time | forward-confirmed reverse DNS |
| **Windows** | WFP filters scoped to the child's AppContainer SID | pre-resolved at spawn | not yet (fail-closed) |

The **access decision is identical** — a binary can reach exactly the hosts/IPs your grants cover and nothing else. Two mechanism-level nuances to know:

- **macOS wildcards** rely on the destination having correct reverse DNS (true for most services). A host whose PTR record doesn't confirm back to its address won't match a wildcard grant there; grant it by concrete name or IP instead.
- **Windows** currently pre-resolves names once at spawn (a brief staleness window on round-robin/TTL) and does **not** yet honor wildcard host grants — a wildcard-granted connection fails closed. Full Windows parity would need a per-connection relay like the macOS backend.

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

# Permissions

agent.d's permission engine is default-deny. No action can touch the local system, network, secrets, or AI providers unless every layer of the permission stack allows it. This page explains the model so you can reason about access decisions and debug them.

## The five-layer intersection

Every action call is evaluated through five layers. All five must pass — a denial at any layer blocks the call:

```text
tool/package grants
  ∩ action.requires
  ∩ runner.allow
  ∩ interface.allow
  ∩ policy
  = Decision
```

| Layer | What it checks | Configured in |
|---|---|---|
| **1. Tool / package grants** | Does the tool (or package) have the capability slug granted? | `grants.toml` `[tool.*]` / `[package.*]` |
| **2. Action requires** | Does the action declare it needs this slug? | `action.requires` in Lua |
| **3. Runner allowlist** | If called from a runner, is this action in the runner's allowlist? | `grants.toml` `[runner.*].allowed_actions` |
| **4. Interface allowlist** | If called from an interface, is this action in the interface's allowlist? | `grants.toml` `[interface.*].allowed_actions` |
| **5. Policy** | Is the action or permission hard-denied by policy? | `grants.toml` `[policy]` |

A runner or interface with no `allowed_actions` entry has **no constraint at that layer** — the other layers still apply.

## grants.toml is the only source of grants

Component manifests declare what they need — they never grant anything. The `requires` field on a tool or action is documentation for operators and the package system. Access is only conferred by `grants.toml`:

```toml
# Layer 1: grant the git tool the ability to run the git binary
[tool.git]
granted = ["shell.exec:git"]

# Layer 3: restrict the reviewer runner to only these two actions
[runner.backend_reviewer]
allowed_actions = ["git.diff", "git.status"]

# Layer 4: restrict a hypothetical Telegram interface
[interface.telegram]
allowed_actions = ["git.status"]

# Service: its own granted capabilities + optional action allowlist
[service.discord_handler]
granted         = ["ai:openai", "net:discord.com", "memory.read:discord/**", "memory.write:discord/**"]
allowed_actions = ["discord.send"]

# Approve a whole installed package's declared permission set
[package.acme]
trusted = true

# Layer 5: hard denials and confirm pre-approvals
[policy]
deny_actions     = ["shell.exec"]
deny_permissions = []
auto_confirm     = []
```

## Permission slugs

Slugs have the shape `domain[:specifier]`. Wildcards are supported on the specifier:

| Slug | What it covers |
|---|---|
| `shell.exec` | Run any process via `ctx.shell`. |
| `shell.exec:git` | Run only the `git` binary (first arg to `ctx.shell`). |
| `net:<host>` | HTTP or WebSocket to that host. |
| `net:*` | HTTP or WebSocket to any host. |
| `fs.read:<glob>` | Filesystem reads matching the glob. |
| `fs.write:<glob>` | Filesystem writes matching the glob. |
| `secret:<key>` | Keyring access for a specific key. |
| `secret:*` | Keyring access for any key. |
| `memory.read:<ns-glob>` | Read from durable memory namespaces matching the glob. |
| `memory.write:<ns-glob>` | Write to durable memory namespaces matching the glob. |
| `ai:<provider>` | Model calls through a specific provider prefix. |
| `ai:*` | Model calls through any provider. |
| `oauth:<provider>` | OAuth grant for a provider. |

Glob matching on `fs.*` slugs is path-aware (`fs.write:/tmp/**` covers any path under `/tmp/`).

## Filesystem path resolution

Before checking a `fs.read` or `fs.write` grant, the runtime resolves the requested path — following symlinks and collapsing `..` segments. This means path-scoped grants cannot be bypassed through aliases or traversal tricks. A grant of `fs.read:/workspace/**` does not cover `/etc/passwd` even if a symlink inside `/workspace` points there.

## How each caller type contributes layers

| Caller | Layer 1 | Layer 2 | Layer 3 | Layer 4 | Layer 5 |
|---|---|---|---|---|---|
| WebSocket client → action | tool grants | action.requires | — | interface.allowed_actions | policy |
| Runner tool-use step | tool grants | action.requires | runner.allowed_actions | interface.allowed_actions | policy |
| Service → capability | service.granted | — | service.allowed_actions (optional) | — | policy |
| Action → action (`ctx.call`) | tool grants | action.requires | inherited runner allow | inherited interface allow | policy |

## Manifests declare, grants.toml grants

A package's `package.toml` (or a tool's `requires`) declares permissions — this tells you and the installer what the component needs. But declaration alone grants nothing. To activate a package's declared permission set, you must trust it:

```toml
[package.acme]
trusted = true
```

Without this, the package contributes zero grants and all its actions fail at layer 1.

## Policy: hard denials

`[policy].deny_actions` and `deny_permissions` are absolute — they are never escalated to interactive approval. Use them to enforce invariants that must hold regardless of what any caller requests:

```toml
[policy]
deny_actions = ["shell.exec"]   # never allow raw shell execution
```

## `confirm` actions and interactive approvals

When an action is declared with `confirm = true`, each call requires explicit operator approval (unless `auto_confirm` covers it). The runtime suspends the call and sends an approval request to the `/control` plane:

```lua
agentd.action({
  name    = "deploy.preview",
  confirm = true,
  handler = function(args, ctx) ... end,
})
```

An operator running `agentctl grants listen` sees the request and chooses:
- **once** — allow this call only.
- **forever** — add to `auto_confirm` in `grants.toml` and reload.
- **deny** — reject the call.

If no operator is connected before `approval_timeout_ms` (default 120 s), the request fails closed with a denial.

Policy denials and allowlist denials are **not** escalated to approval — they are hard stops.

::: warning
`allow_forever` from the approval console writes to `grants.toml` automatically. Review it after a session if you granted anything you want to make permanent.
:::

## See also

- [Security: grants](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)
- [Interactive approvals](/v0/security/approvals)
- [Tutorial: permissions](/v0/tutorial/permissions)

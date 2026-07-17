# Step 3 — Permissions

Without a grants file every call to your new tool fails. This page explains agent.d's default-deny permission engine and shows you the minimal `grants.toml` that lets the git tool run.

## Why default-deny?

agent.d's permission engine evaluates five layers in intersection order:

```
tool/package grants
  ∩ action.requires
  ∩ runner.allowed_actions
  ∩ interface.allowed_actions
  ∩ policy
  = Decision
```

A call is allowed only when **every** layer that applies says yes. If any layer denies — or has no entry — the call is blocked. Writing `requires` in your Lua declares what an action needs; it never grants that need.

`grants.toml` is **the only source of grants**.

## Write `grants.toml`

Create `grants.toml` in your project root:

```toml
# grants.toml

[tool.git]
granted = ["shell.exec:git"]
```

That one line is enough to let `agentctl call git.status` succeed. The slug `shell.exec:git` matches the `bin` argument you pass to `ctx.shell` — the bare binary name `"git"`.

## Permission slug anatomy

Slugs follow the form `domain` or `domain:specifier`. The domains relevant to this project:

| Slug | What it allows |
|------|---------------|
| `shell.exec:git` | Run the `git` binary via `ctx.shell`. |
| `ai:anthropic` | Make model calls through the Anthropic provider. |
| `net:<host>` | HTTP or WebSocket to a specific host. |
| `fs.read:<glob>` | Filesystem reads under a path glob. |
| `fs.write:<glob>` | Filesystem writes under a path glob. |

Wildcards are supported on the specifier: `net:*` allows all hosts, `fs.write:/tmp/**` allows all writes under `/tmp`.

::: info Filesystem grants are resolved
Path globs in `fs.read` and `fs.write` grants are resolved (symlinks, `..` expanded) before matching, so a tool cannot reach past them with aliases.
:::

## The other grant sections

Your `grants.toml` can grow to cover every layer of the engine:

```toml
# Per-tool: what the tool itself is allowed to do.
[tool.git]
granted = ["shell.exec:git"]

# Per-runner: which actions the runner may call.
# Empty section = no constraint at this layer.
[runner.backend_reviewer]
allowed_actions = ["git.diff", "git.status"]

# Per-interface: which actions a connected client may call.
[interface.telegram]
allowed_actions = ["git.status"]

# Final policy denylist (hard block, never escalated to approval).
[policy]
deny_actions = []
deny_permissions = []
auto_confirm = []
```

For now the minimal one-section file is all you need. You will add the runner section in the next step.

::: warning `requires` is not self-granting
A component's `requires` list is a declaration of need, not a grant. Removing the `[tool.git]` block from `grants.toml` while keeping `requires = { "shell.exec:git" }` in Lua still denies every call.
:::

## Next step

[Step 4 — Runner and skill →](/v0/tutorial/runner-and-skill)

## See also

- [Concepts: permissions](/v0/concepts/permissions)
- [Security: grants](/v0/security/grants)
- [Security: permission slugs](/v0/security/permission-slugs)
- [Security: approvals](/v0/security/approvals)

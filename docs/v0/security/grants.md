# grants.toml reference

`grants.toml` is the only source of capability grants in agent.d. Every component declares what it _needs_ in its manifest, but nothing is ever self-granted — this file is where you, the operator, approve those needs.

## Location and loading

The default path is `$XDG_CONFIG_HOME/agentd/grants.toml`. Override it with `--grants-file <path>` or `AGENTD_GRANTS_FILE`. The file is reloaded automatically when you run with `--watch`.

`init.lua` and `grants.toml` live in the same folder - that folder is serves as the **workspace root**. Supplying one option lets agent.d infer the other: pass just `--init path/to/init.lua` and it reads `grants.toml` beside it, or pass just `--grants-file path/to/grants.toml` and it loads `init.lua` beside it. You only need both flags if they live in different folders.

## The five-layer engine

Every action call passes through five intersecting layers in order:

```
tool/package grants
        ∩ action.requires
        ∩ runner.allowed_actions
        ∩ interface.allowed_actions
        ∩ policy
        = Decision
```

The result is **default-deny**: a call succeeds only when every applicable layer permits it. `grants.toml` controls the first and last layers.

## Schema

### `[tool.<name>]`

Grants capability slugs to every action registered under that tool.

```toml
[tool.git]
granted = ["shell.exec:git"]
```

The tool name matches the `name` field passed to `agentd.tool{...}` in your Lua entry.

### `[runner.<name>]`

Restricts which actions a runner may call. An **empty list means no constraint at this layer** — all actions the runner's other grants permit are allowed.

A runner that makes model calls also needs a `granted` entry for the `ai:` permission:

```toml
[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]
```

### `[interface.<name>]`

Restricts which actions a connected interface (e.g. a WebSocket client) may call. An empty list means no constraint at this layer. The `granted` field is also accepted and works the same way as for runners — use it to grant capability slugs to calls arriving through this interface.

```toml
[interface.telegram]
allowed_actions = ["git.status"]
```

### `[service.<name>]`

Services have their own capability grants plus an optional action allowlist.

```toml
[service.discord_gateway]
granted = [
  "net:gateway.discord.gg",
  "net:discord.com",
  "secret:discord_token",
]

[service.discord_handler]
granted = [
  "ai:openai",
  "net:discord.com",
  "memory.read:discord/**",
  "memory.write:discord/**",
]
allowed_actions = ["discord.send"]
```

### `[package.<name>]`

Approves the entire declared permission set of an installed package. Without this entry the package contributes zero grants.

```toml
[package.acme]
trusted = true
```

When `trusted = true`, every component the package registers (all auto-prefixed `acme/...`) inherits the permissions listed in the package's `package.toml`. You can still narrow a specific component by writing an explicit entry such as `[tool."acme/git"]` — an explicit entry always takes precedence over the inherited package grant.

### `[policy]`

The final denylist layer and confirm pre-approvals.

```toml
[policy]
deny_actions     = ["shell.exec"]   # hard-deny these actions (never escalated)
deny_permissions = []               # hard-deny these permission slugs
auto_confirm     = []               # pre-approve confirm = true actions
```

::: warning
`deny_actions` and `deny_permissions` are **hard denials** — they are never escalated to interactive approval. Use them to permanently block dangerous operations.
:::

`auto_confirm` lists actions whose `confirm = true` gate is pre-approved. `agentctl grants listen` appends here automatically when you choose "allow forever" on a confirm-gated action, but you can also hand-edit the list.

## Full annotated example

```toml
# tool grants
[tool.git]
granted = ["shell.exec:git"]

# runner action allowlist (empty = no constraint)
[runner.backend_reviewer]
allowed_actions = ["git.diff", "git.status"]

# interface action allowlist
[interface.telegram]
allowed_actions = ["git.status"]

# service grants + allowlist
[service.discord_handler]
granted = [
  "ai:openai",
  "net:discord.com",
  "memory.read:discord/**",
  "memory.write:discord/**",
]
allowed_actions = ["discord.send"]

# package approval
[package.acme]
trusted = true

# policy denylist
[policy]
deny_actions     = []
deny_permissions = []
auto_confirm     = []
```

## Key rules to remember

- A component's `requires` field **declares** needs; it never grants anything.
- An explicit `[tool.<name>]` or `[service.<name>]` entry always overrides an inherited package grant for that specific component.
- An empty `allowed_actions` list means **no constraint** at that layer, not "block everything".
- Filesystem path grants are resolved (symlinks, `..` segments) before checking, so aliases cannot bypass a glob restriction.
- A **relative** `fs.read`/`fs.write` path grant resolves against the granted component's working directory (the workspace root by default), so `fs.read:notes/**` is portable across machines. An absolute grant is honored as-is. A bare `shell.exec:python` stays a `PATH` lookup — only `shell.exec` specifiers containing a path separator are treated as cwd-relative.
- Specifiers support wildcards: `net:*`, `fs.write:tmp/**`, `memory.read:discord/**`.

## See also

- [Permission slugs](/v0/security/permission-slugs)
- [Interactive approvals](/v0/security/approvals)
- [Permissions concept](/v0/concepts/permissions)
- [Best practices](/v0/security/best-practices)

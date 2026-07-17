# Permission slugs

Every capability in agent.d is identified by a **permission slug** of the form `domain` or `domain:specifier`. You use slugs in `requires` arrays (to declare needs) and in `grants.toml` (to approve them).

## Syntax

```
domain[:specifier]
```

- **`domain`** — the capability class (e.g. `shell.exec`, `net`, `fs.read`).
- **`specifier`** — an optional qualifier that narrows the grant to a specific target. Specifiers support wildcards using `**` glob syntax.

A grant without a specifier covers the whole domain only when the domain does not define a specifier. For domains that do define specifiers (such as `net` and `fs.*`), you must include a specifier in the grant for it to be useful.

## Domains

### `shell.exec`

Run a child process via `ctx.shell`. The specifier is the **bare binary name** — the first argument passed to `ctx.shell`, not a full path.

| Slug | Meaning |
|---|---|
| `shell.exec` | Run any binary |
| `shell.exec:git` | Run `git` only |
| `shell.exec:npm` | Run `npm` only |

```toml
[tool.git]
granted = ["shell.exec:git"]
```

::: tip
Prefer `shell.exec:<bin>` over the bare `shell.exec`. Scoping to a specific binary is one of the most effective least-privilege controls available.
:::

### `net`

Make HTTP or WebSocket requests via `ctx.http` or `ctx.ws`. The specifier is the **hostname**.

| Slug | Meaning |
|---|---|
| `net:api.example.com` | Requests to `api.example.com` only |
| `net:discord.com` | Requests to `discord.com` |
| `net:*` | Requests to any host |

```toml
[service.discord_gateway]
granted = ["net:gateway.discord.gg", "net:discord.com"]
```

### `fs.read` / `fs.write`

Read or write files via `ctx.fs`. The specifier is a **glob path**.

| Slug | Meaning |
|---|---|
| `fs.read:/home/user/data/**` | Read any file under that directory |
| `fs.write:/tmp/**` | Write any file under `/tmp` |
| `fs.read:/etc/config.toml` | Read one specific file |

```toml
[tool.site]
granted = ["fs.write:/tmp/agentd-smoke/site/**"]
```

::: warning
Filesystem path grants are **resolved** (symlinks and `..` segments expanded) before matching. A symlink or `../` traversal cannot be used to escape a glob restriction.
:::

### `secret`

Access the OS keyring via `ctx.secret`. The specifier is the **key name**.

| Slug | Meaning |
|---|---|
| `secret:discord_token` | Access the `discord_token` key only |
| `secret:*` | Access any key in the keyring |

```toml
[tool.discord]
granted = ["secret:discord_token"]
```

### `memory.read` / `memory.write`

Read or write durable memory namespaces via `ctx.memory`. The specifier is a **namespace glob**.

| Slug | Meaning |
|---|---|
| `memory.read:discord/**` | Read any key in any `discord/…` namespace |
| `memory.write:discord/**` | Write any key in any `discord/…` namespace |
| `memory.read:ns/**` | Read any key under `ns/` |

```toml
[service.discord_handler]
granted = [
  "memory.read:discord/**",
  "memory.write:discord/**",
]
```

### `ai`

Gates model calls. The specifier is the **provider prefix** (the part before
the `/` in a `"<provider>/<model_id>"` string).

This covers both direct calls through `ctx.ai` **and** running a runner: a
caller that invokes `ctx.run` (or `runners.run`) needs the `ai:` grant for that
runner's model provider. In the Discord example below, the service holds
`ai:openai` because it runs a runner whose model is `openai/gpt-5.5`.

The specifier must match the prefix exactly — `ai:anthropic` does **not** grant
`anthropic-cli/…`. Each backend is its own slug:

| Slug | Grants the prefix |
|---|---|
| `ai:anthropic` | `anthropic/…` |
| `ai:anthropic-cli` | `anthropic-cli/…` |
| `ai:openai` | `openai/…` |
| `ai:openai-cli` | `openai-cli/…` |
| `ai:codex` | `codex/…` |
| `ai:<name>` | `<name>/…` for any [`[providers.<name>]`](/v0/providers/custom) entry, e.g. `ai:ollama` |
| `ai:*` | any registered provider |

```toml
[service.discord_handler]
granted = ["ai:openai"]   # runs a runner on openai/gpt-5.5
```

### `oauth`

OAuth grant for a provider. The specifier is the **provider name**. Declare this domain in `requires` when your component initiates an OAuth flow.

| Slug | Meaning |
|---|---|
| `oauth:github` | OAuth grant for GitHub |

## Wildcard rules

- `*` matches any single path segment or any single value (e.g. `net:*` matches all hostnames).
- `**` matches zero or more path segments in filesystem and memory globs (e.g. `fs.read:/data/**`, `memory.read:ns/**`).
- Path grants are always resolved before matching — a tool cannot reach past a glob with aliases or `..` traversal.

## See also

- [grants.toml reference](/v0/security/grants)
- [Interactive approvals](/v0/security/approvals)
- [Permissions concept](/v0/concepts/permissions)
- [Best practices](/v0/security/best-practices)

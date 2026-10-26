# Security best practices

This page collects operator guidance for running agent.d securely. The permission engine is default-deny, but a misconfigured `grants.toml` can still expose more capability than intended.

## Grant the minimum necessary

Prefer narrow slugs over broad ones. Every slug you broaden is a larger attack surface if a tool or action is exploited.

```toml
# Too broad — lets the git tool run any binary
[tool.git]
granted = ["shell.exec"]

# Better — limits execution to the `git` binary only
[tool.git]
granted = ["shell.exec:git"]
```

Apply the same principle to all domains:

| Domain | Broad (avoid) | Narrow (prefer) |
|---|---|---|
| Shell | `shell.exec` | `shell.exec:git` |
| Network | `net:*` | `net:api.example.com` |
| Filesystem | `fs.write:/**` | `fs.write:/tmp/myapp/**` |
| Memory | `memory.read:*` | `memory.read:discord/**` |
| AI | `ai:*` | `ai:anthropic` |
| Secrets | `secret:*` | `secret:discord_token` |

## Keep secrets in the keyring

Never put tokens, API keys, or passwords in `grants.toml`, `init.lua`, or any source file. Store them with `agentctl call` (or a dedicated `set_token` action), then retrieve them at runtime with `ctx.secret.get`:

```lua
local token = ctx.secret.get("discord_token")
-- permission: secret:discord_token
```

The keyring is backed by the OS credential store and is not persisted in files checked into version control.

## Use separate tokens for `/ws` and `/control`

The `/ws` and `/control` endpoints accept different bearer tokens (`AGENTD_TOKEN` and `AGENTD_ADMIN_TOKEN` respectively). Issue distinct tokens for each surface:

- Give clients the `/ws` token only.
- Keep the admin token restricted to operators. It answers approval requests on `/control`, and on `/ws` it can make approval looser for a run. See [Admin connections](/v0/security/approvals#admin-connections).

Never share the admin token with untrusted clients.

## Bind to localhost

The daemon defaults to `127.0.0.1:7777`. Do not bind to `0.0.0.0` or a public interface unless you have a reverse proxy with TLS and authentication in front of it.

```toml
[daemon]
addr = "127.0.0.1:7777"   # default; keep it
```

## Use `confirm = true` for dangerous actions

Mark actions that modify critical state, call external APIs with side effects, or run privileged commands with `confirm = true`. This requires an operator to explicitly approve each call:

```lua
agentd.action({
  name = "git.push",
  handler = push_handler,
  requires = { "shell.exec:git" },
  confirm = true,   -- requires interactive approval on every call
})
```

Pre-approve an action only after you are sure it is safe in your environment. To do that, add it to `[approval].skip`.

## Use `[denial]` for permanent blocks

If an action or a permission must *never* be used, whatever `grants.toml` grants, add it to `[denial]`:

```toml
[denial]
actions     = ["pkg.publish"]                   # never publish a package
permissions = ["fs.read:/home/me/project/.env"] # never read this file
```

Nobody is asked about a denial, and nobody can approve it. Keep denied files outside the folders that you grant to shell commands. Otherwise, agent.d must refuse those commands, because a sandbox cannot hide one file in a folder that it opens. See [What a denial guarantees](/v0/security/denials).

## Choose who answers for each runner

Set `approval` for each runner in `grants.toml`:

- Use `"nobody"` for bots and other runs that nobody watches. The run can use only what you granted, and it never waits for an answer.
- Use `"classifier"` when an agent must continue without you, but each new step needs a check. Give the classifier clear instructions about what to allow.
- Keep `"human"` when you want to decide each step yourself.

```toml
[runner.discord_bot]
granted  = ["ai:openai"]
approval = "nobody"
```

A client can make a run stricter, but only an admin connection can make it looser. So a runner set to `"nobody"` stays safe even when a client asks for more. See [Interactive approvals](/v0/security/approvals#who-answers).

## Review package permission sets before trusting

When you install a package, agent.d reports its declared `permissions` from `package.toml`. Read that list carefully before adding `trusted = true` in `grants.toml`. If the package needs more than you expect, investigate before approving.

```toml
# Only add this after reviewing the declared permissions
[package.acme]
trusted = true
```

You can also narrow a specific component within a trusted package by adding an explicit entry that overrides the inherited grant:

```toml
[package.acme]
trusted = true

# Override: restrict acme/git to shell.exec:git only, even if acme declares more
[tool."acme/git"]
granted = ["shell.exec:git"]
```

## See also

- [grants.toml reference](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)
- [Interactive approvals](/v0/security/approvals)
- [Shell sandbox](/v0/security/sandbox)

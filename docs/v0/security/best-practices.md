# Security best practices

This page collects advice for running agent.d securely. agent.d is default-deny, but a careless `grants.toml` can still give an agent more than you meant to.

## Grant the minimum necessary

Grant the narrowest slug that does the job. The broader a grant, the more an attacker gains if a tool or action is misused.

```toml
# Too broad — lets the git tool run any binary
[tool.git]
granted = ["shell.exec"]

# Better — limits execution to the `git` binary only
[tool.git]
granted = ["shell.exec:git"]
```

Apply the same rule to every domain:

| Domain | Broad (avoid) | Narrow (prefer) |
|---|---|---|
| Shell | `shell.exec` | `shell.exec:git` |
| Network | `net:*` | `net:api.example.com` |
| Filesystem | `fs.write:/**` | `fs.write:/tmp/myapp/**` |
| Memory | `memory.read:*` | `memory.read:discord/**` |
| AI | `ai:*` | `ai:anthropic` |
| Secrets | `secret:*` | `secret:discord_token` |

## Keep secrets in the keyring

Keep tokens, API keys and passwords out of `grants.toml`, `init.lua` and every other source file. Store them with `agentctl secret set`, then read them at runtime with `ctx.secret.get`:

```lua
local token = ctx.secret.get("discord_token")
-- permission: secret:discord_token
```

The keyring is your operating system's credential store, so the values stay out of the files you check into version control.

## Use separate tokens for `/ws` and `/control`

The `/ws` and `/control` endpoints accept different bearer tokens: `AGENTD_TOKEN` and `AGENTD_ADMIN_TOKEN`. Give each a distinct value:

- Give clients the `/ws` token only.
- Give the admin token only to the people who run the daemon. It answers approval requests on `/control`, and on `/ws` it can make approval looser for a run. See [Admin connections](/v0/security/approvals#admin-connections).

## Bind to localhost

The daemon listens on `127.0.0.1:7777` by default, which only your machine can reach. Bind to `0.0.0.0` or a public interface only behind a reverse proxy with TLS and authentication.

```toml
[daemon]
addr = "127.0.0.1:7777"   # default; keep it
```

## Use `confirm = true` for dangerous actions

Mark actions that change critical state, call external APIs with side effects, or run privileged commands with `confirm = true`. Each call then needs an approval:

```lua
agentd.action({
  name = "git.push",
  handler = push_handler,
  requires = { "shell.exec:git" },
  confirm = true,   -- requires interactive approval on every call
})
```

To let a confirm action run without a question, add it to `[approval].skip`. Do this only once you are sure the action is safe in your environment.

## Use `[denial]` for permanent blocks

To block an action or a permission for everyone, whatever `grants.toml` grants, add it to `[denial]`:

```toml
[denial]
actions     = ["pkg.publish"]                   # never publish a package
permissions = ["fs.read:/home/me/project/.env"] # never read this file
```

A denied call is refused without a question, so nobody can approve it. Keep denied files outside the folders that you grant to shell commands. A sandbox cannot hide one file inside a folder it opens, so agent.d refuses any shell command whose folders contain a denied file. See [What a denial guarantees](/v0/security/denials).

## Choose who answers for each runner

Set `approval` for each runner in `grants.toml`:

- Use `"nobody"` for bots and other runs that nobody watches. The run can use only what you granted, and anything else is refused at once.
- Use `"classifier"` when an agent should keep working without you and each ungranted step still needs a check. Give the classifier clear instructions about what to allow.
- Keep `"human"` when you want to decide each step yourself.

```toml
[runner.discord_bot]
granted  = ["ai:openai"]
approval = "nobody"
```

Any client can make a run stricter. Only an admin connection can make it looser, so a runner set to `"nobody"` stays `"nobody"` when a client asks for more. See [Interactive approvals](/v0/security/approvals#who-answers).

## Review package permission sets before trusting

When you install a package, agent.d shows the `permissions` it declares in `package.toml`. Read that list before you add `trusted = true` in `grants.toml`. If the package asks for more than you expect, find out why before you trust it.

```toml
# Only add this after reviewing the declared permissions
[package.acme]
trusted = true
```

To narrow one component of a trusted package, give it its own entry. That entry replaces the package grant for the component:

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

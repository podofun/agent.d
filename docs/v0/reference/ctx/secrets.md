# ctx.secret — Secrets

`ctx.secret` stores and retrieves sensitive values using the OS keyring. Values never appear in logs, memory snapshots, or the trace file.

**Required permission:** `secret:<key>` or `secret:*`.

## Signatures

```lua
ctx.secret.get(key: string) -> string | nil
ctx.secret.set(key: string, value: string)
ctx.secret.delete(key: string)
ctx.secret.exists(key: string) -> boolean
ctx.secret.list() -> string[]
```

## Methods

| Method | Permission | Description |
|---|---|---|
| `ctx.secret.get(key)` | `secret:<key>` | Retrieve the secret value for `key`. Returns `nil` if the key does not exist. |
| `ctx.secret.set(key, value)` | `secret:<key>` | Store `value` under `key` in the OS keyring. |
| `ctx.secret.delete(key)` | `secret:<key>` | Remove the key from the keyring. |
| `ctx.secret.exists(key)` | `secret:<key>` | Return `true` if the key exists in the keyring. |
| `ctx.secret.list()` | `secret:*` | Return the names of all stored keys. |

## Parameters

| Parameter | Type | Description |
|---|---|---|
| `key` | `string` | Unique identifier for the secret. Convention: `snake_case`, e.g. `discord_token`. |
| `value` | `string` | The secret string to store. |

## Permission

Grant the exact key or a wildcard in `grants.toml`:

```toml
[tool.discord]
granted = ["secret:discord_token"]

[service.discord_handler]
granted = ["secret:discord_token"]
```

## Seeding secrets via agentctl

Secrets are typically seeded at setup time with [`agentctl secret set`](/v0/reference/cli#agentctl-secret-set), which writes to the same keyring:

```bash
echo "$DISCORD_TOKEN" | agentctl secret set discord_token
```

No Lua action or grant is needed for this — reserve `ctx.secret.set` for secrets your tools and services manage at runtime.

## Examples

```lua
-- Store a token during first-time setup
agentd.action("discord.set_token", function(args, ctx)
  ctx.secret.set("discord_token", args.token)
  return "token saved"
end)
```

```lua
-- Retrieve a token at runtime
agentd.service("discord_gateway", function(ctx)
  local token = ctx.secret.get("discord_token")
  if not token then
    error("discord_token not set — run discord.set_token first")
  end
  -- use token ...
end)
```

```lua
-- Check before use, avoid silent nil errors
agentd.action("github.status", function(args, ctx)
  if not ctx.secret.exists("github_token") then
    error("github_token not configured")
  end
  local token = ctx.secret.get("github_token")
  local res = ctx.http.get("https://api.github.com/user", {
    headers = { Authorization = "Bearer " .. token },
  })
  return res:json()
end)
```

## See also

- [Security: best practices](/v0/security/best-practices)
- [Security: permission slugs](/v0/security/permission-slugs)
- [ctx.http](/v0/reference/ctx/http)
- [Recipes: discord-bot](/v0/recipes/discord-bot)

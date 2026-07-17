# Credentials

agent.d providers that call external APIs read their keys from the **OS keyring** (secret store). This page explains how to store credentials and what grants are required to access them.

## How credentials work

The daemon uses the OS keyring (via the `agentd-secrets` crate backed by `KeyringStore`) as the single source of truth for provider API keys. When a provider like `anthropic` or `openai` makes a model call, it retrieves its key from the keyring at call time.

## Storing a key with `agentctl secret`

The primary way to manage provider keys is the `agentctl secret` command. It writes to the same `agentd` keyring service the daemon reads, so a running daemon sees changes immediately — no restart, no setup action:

```bash
agentctl secret set anthropic_api_key sk-ant-…
```

To keep the key out of your shell history, omit the value and pipe it on stdin:

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

On success it prints:

```
stored `anthropic_api_key` — available to the daemon immediately
```

To verify what is stored without exposing it, or to remove a key:

```bash
agentctl secret peek anthropic_api_key
# sk-a************nt- (32 chars)

agentctl secret unset anthropic_api_key
```

See the [CLI reference](/v0/reference/cli#agentctl-secret-set) for the full command surface.

::: warning Never hardcode keys
Do not put API keys in `init.lua`, `config.toml`, environment variables checked into version control, or any file committed to a repository. The keyring is the only safe place for secrets in agent.d.
:::

## The `ctx.secret` API (programmatic)

When a tool or service needs to read or write secrets at runtime — bot tokens, per-user credentials, keys received over an API — use `ctx.secret` from Lua. It gives you full CRUD over the same keyring. Every call requires the `secret:<key>` grant (or `secret:*` to allow all keys).

```lua
ctx.secret.set(key, value)          -- write a secret
ctx.secret.get(key) -> string|nil   -- read a secret (nil if not found)
ctx.secret.exists(key) -> boolean   -- check without reading
ctx.secret.delete(key)              -- remove a secret
ctx.secret.list() -> string[]       -- list all stored key names
```

For example, an action that stores a token handed to it as an argument:

```lua
agentd.action("discord.set_token", function(args, ctx)
  ctx.secret.set("discord_token", args.token)
  return "stored"
end)
```

For one-off provider key setup you do not need any of this — `agentctl secret set` does the same thing with no Lua and no grant wiring. See [ctx.secret](/v0/reference/ctx/secrets) for the full reference.

## Required grants

Two grant domains are relevant for credentials:

| Grant | What it allows |
|---|---|
| `secret:<key>` | Read, write, or delete the named key in the keyring |
| `secret:*` | Access any key (use sparingly) |
| `ai:<provider>` | Make model calls through the named provider |

`agentctl secret` operates directly on the keyring and needs no grants. Grants apply to Lua code — grant them in `grants.toml` only to the tools or services that genuinely need them:

```toml
# grants.toml

[tool.discord]
granted = ["secret:discord_token"]

[tool.review]
granted = ["ai:anthropic"]

[service.discord_handler]
granted = ["ai:openai", "secret:discord_token"]
```

::: info Principle of least privilege
Grant `secret:<specific-key>` rather than `secret:*`. Grant `ai:<specific-provider>` rather than `ai:*`. Narrow grants limit blast radius if a component is compromised or misbehaves.
:::

## Rotating a key

To rotate a provider key, run `agentctl secret set` again with the new value — it overwrites the existing entry. No daemon restart is required; the provider reads the key on each call.

```bash
echo "$NEW_KEY" | agentctl secret set anthropic_api_key
```

## See also

- [`agentctl secret`](/v0/reference/cli#agentctl-secret-set) — CLI reference for set/unset/peek
- [ctx.secret](/v0/reference/ctx/secrets) — full `ctx.secret` API reference
- [Security best practices](/v0/security/best-practices) — broader guidance on secrets handling
- [Permission slugs](/v0/security/permission-slugs) — `secret:*` and `ai:*` slug domains
- [Providers overview](/v0/providers/) — which providers use keyring-stored keys

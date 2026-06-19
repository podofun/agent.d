# Credentials

agent.d providers that call external APIs read their keys from the **OS keyring** (secret store) — never from environment variables, config files, or hardcoded Lua strings. This page explains how to store credentials and what grants are required to access them.

## How credentials work

The daemon uses the OS keyring (via the `agentd-secrets` crate backed by `KeyringStore`) as the single source of truth for provider API keys. When a provider like `anthropic` or `openai` makes a model call, it retrieves its key from the keyring at call time.

You write keys into the keyring using `ctx.secret` from a Lua action:

```lua
-- A one-time setup action. Call it once via agentctl, then remove or restrict it.
agentd.action("setup.store_key", function(args, ctx)
  ctx.secret.set(args.name, args.value)
  return "stored"
end)
```

Then seed the key:

```bash
agentctl call setup.store_key -d name=anthropic_api_key -d value=sk-ant-…
```

::: warning Never hardcode keys
Do not put API keys in `init.lua`, `config.toml`, environment variables checked into version control, or any file committed to a repository. The keyring is the only safe place for secrets in agent.d.
:::

## The `ctx.secret` API

`ctx.secret` gives you full CRUD over the keyring from Lua. Every call requires the `secret:<key>` grant (or `secret:*` to allow all keys).

```lua
ctx.secret.set(key, value)          -- write a secret
ctx.secret.get(key) -> string|nil   -- read a secret (nil if not found)
ctx.secret.exists(key) -> boolean   -- check without reading
ctx.secret.delete(key)              -- remove a secret
ctx.secret.list() -> string[]       -- list all stored key names
```

See [ctx.secret](/v0/reference/ctx/secrets) for the full reference.

## Required grants

Two grant domains are relevant for credentials:

| Grant | What it allows |
|---|---|
| `secret:<key>` | Read, write, or delete the named key in the keyring |
| `secret:*` | Access any key (use sparingly) |
| `ai:<provider>` | Make model calls through the named provider |

Grant these in `grants.toml` only to the tools or services that genuinely need them:

```toml
# grants.toml

[tool.setup]
granted = ["secret:anthropic_api_key"]

[tool.review]
granted = ["ai:anthropic"]

[service.discord_handler]
granted = ["ai:openai", "secret:discord_token"]
```

::: info Principle of least privilege
Grant `secret:<specific-key>` rather than `secret:*`. Grant `ai:<specific-provider>` rather than `ai:*`. Narrow grants limit blast radius if a component is compromised or misbehaves.
:::

## Rotating a key

To rotate a provider key, call `ctx.secret.set` again with the new value — it overwrites the existing entry. No daemon restart is required; the provider reads the key on each call.

```bash
agentctl call setup.store_key -d name=anthropic_api_key -d value=sk-ant-new-…
```

## See also

- [ctx.secret](/v0/reference/ctx/secrets) — full `ctx.secret` API reference
- [Security best practices](/v0/security/best-practices) — broader guidance on secrets handling
- [Permission slugs](/v0/security/permission-slugs) — `secret:*` and `ai:*` slug domains
- [Providers overview](/v0/providers/) — which providers use keyring-stored keys

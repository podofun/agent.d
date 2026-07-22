# Recipes

These self-contained configurations are ready to copy and run. Each recipe includes the Lua, a `grants.toml`, run instructions, and a verification step.

| Recipe | What it shows |
|---|---|
| [Code review runner](/v0/recipes/code-review) | `ctx.shell` + a Markdown skill + a runner; invoke with `agentctl runner run` |
| [Discord bot](/v0/recipes/discord-bot) | Two services, WebSocket gateway with heartbeat, named channel, per-channel durable memory, REST client, secret-stored token |
| [HTTP tool](/v0/recipes/http-tool) | `ctx.http.client` calling an external JSON API; `net:<host>` grant; invoke with `agentctl call` |
| [Webhook trigger](/v0/recipes/webhook) | Trigger an action from an external system over the `/ws` data plane; read caller identity via `ctx.caller` |
| [Per-user memory](/v0/recipes/per-user-memory) | Rolling-window durable history keyed by caller; `memory.read/write` grants |

## See also

- [Concepts overview](/v0/concepts/)
- [Writing tools](/v0/writing/tools)
- [Writing runners](/v0/writing/runners)
- [Permission slugs](/v0/security/permission-slugs)

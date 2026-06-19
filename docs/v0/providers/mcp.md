# MCP loopback

agent.d includes a **loopback MCP server** that lets a CLI-driven provider (such as `anthropic-cli`) call back into the daemon's action registry while the upstream CLI is running the agent loop. This bridges the Claude CLI's native tool-use machinery with agent.d's permission engine.

## What it enables

When you run a runner backed by a CLI provider, agent.d:

1. Binds a short-lived HTTP JSON-RPC server on `127.0.0.1` (random port).
2. Passes the server URL to the CLI via its `--mcp-config` flag.
3. The CLI discovers the runner's allowed actions as MCP tools and calls them during the agentic loop.
4. Each tool call is dispatched back through the agent.d executor — the full permission engine runs on every call, exactly as it would for a direct `ctx.call`.
5. The loopback server is torn down as soon as the runner call completes.

The result: the upstream CLI handles the conversation loop and model API calls, while agent.d remains the authority on what actions the model is allowed to invoke and how they execute.

::: info Per-invocation isolation
Each runner call gets its own loopback listener with its own ephemeral bearer token. The listener only exposes the actions listed in the runner's `actions` allowlist, and it shuts down the moment the call finishes. No MCP state is shared across invocations.
:::

## Permission enforcement

The loopback server enforces the runner's action catalog first (calls for unlisted actions are rejected before reaching the executor), then passes through the dispatcher where the full five-layer permission engine applies — runner grants, tool grants, policy denials, and approval flows all remain in effect.

## When you encounter this

You do not configure the MCP loopback directly. It is an implementation detail of the `anthropic-cli` provider (and potentially `codex` in future). If you are using `anthropic-cli` runners and see connection activity on `127.0.0.1` at a high ephemeral port, that is the loopback server for an active runner invocation.

::: warning Local-only
The loopback server binds on `127.0.0.1` only and is never reachable from the network. It is not the same as agent.d's main `/ws` WebSocket endpoint.
:::

## See also

- [CLI backends](/v0/providers/cli-backends) — `anthropic-cli` and `openai-cli` providers that use MCP
- [Runners](/v0/concepts/runners) — composing models, skills, and action allowlists
- [Security grants](/v0/security/grants) — how runner action allowlists constrain tool-use
- [Providers overview](/v0/providers/) — all registered provider prefixes

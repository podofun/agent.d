# CLI backends

agent.d ships two local CLI backends that drive model calls through a locally installed CLI tool rather than a direct API call. Neither backend requires an API key in the secret store — they delegate authentication to the CLI tool itself.

| Prefix | CLI tool | Notes |
|---|---|---|
| `anthropic-cli` | `claude` (Claude CLI) | Drives the local `claude` binary |
| `openai-cli` | `codex` (Codex CLI) | Text-output fallback via the local `codex` binary |

## When to use CLI backends

Use a CLI backend when:

- You have the `claude` or `codex` CLI installed and authenticated, but you have **not** stored an API key in the agent.d keyring.
- You want to use credentials already managed by the upstream CLI (its own keyring / login session).
- You are testing locally and want to avoid provisioning a separate API key for agent.d.

::: info
CLI backends are convenience paths. For production deployments, the direct API backends (`anthropic`, `openai`) with keys stored in the keyring are more reliable and observable.
:::

## anthropic-cli

Uses the locally installed `claude` CLI. The agent.d MCP loopback server is used to feed tool calls back into the daemon during an agentic runner loop — see [MCP loopback](/v0/providers/mcp) for how that works.

```lua
agentd.runner({
  name = "local_reviewer",
  model = "anthropic-cli/claude-opus-4-7",
  actions = { "git.diff", "git.status" },
})
```

**Required permission:** `ai:anthropic-cli`

```toml
[tool.review]
granted = ["ai:anthropic-cli"]
```

## openai-cli

Uses the locally installed `codex` CLI as a text-output fallback. This backend invokes `codex` and captures its text output; it does not drive a full agentic loop the same way `anthropic-cli` does.

```lua
agentd.runner({
  name = "quick_assist",
  model = "openai-cli/",
})
```

**Required permission:** `ai:openai-cli`

```toml
[tool.assist]
granted = ["ai:openai-cli"]
```

::: warning Requires local CLI on PATH
Both backends require the respective CLI binary (`claude` or `codex`) to be installed and on `PATH` where the `agentd` process runs. If the binary is not found, the call will fail at runtime.
:::

## See also

- [Providers overview](/v0/providers/) — all registered prefixes and model selection
- [MCP loopback](/v0/providers/mcp) — how `anthropic-cli` feeds tool calls back into agent.d
- [Credentials](/v0/providers/credentials) — keyring-based credentials for the direct API backends
- [Anthropic provider](/v0/providers/anthropic) — direct Anthropic Messages API backend

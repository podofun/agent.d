# Codex provider

The `codex` prefix connects agent.d to a running **`codex app-server`** process over JSON-RPC. It is distinct from the `openai-cli` text-fallback backend — `codex` speaks directly to the Codex app-server protocol rather than invoking a CLI and capturing its output.

## Using the provider

```lua
agentd.runner({
  name = "codex_agent",
  model = "codex/",
  actions = { "git.diff", "git.status" },
})
```

```lua
local reply = ctx.ai.ask("Explain this code", { model = "codex/" })
```

## Required permission

Any component that calls this provider must hold the **`ai:codex`** grant:

```toml
# grants.toml
[tool.codex_tools]
granted = ["ai:codex"]
```

::: info
The `codex` backend requires `codex app-server` to be running and reachable. Consult the Codex CLI documentation for how to start the app-server.
:::

## See also

- [Providers overview](/v0/providers/) — model selection, max_turns, all registered prefixes
- [CLI backends](/v0/providers/cli-backends) — `openai-cli` text-output fallback via `codex` CLI
- [ctx.ai](/v0/reference/ctx/ai) — full API reference for model calls
- [Security grants](/v0/security/grants) — granting `ai:codex` to components

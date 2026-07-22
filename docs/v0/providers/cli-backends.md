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

Uses the locally installed `claude` CLI. During a runner call, the CLI can call the agent.d actions that the runner permits.

```lua
agentd.runner({
  name = "local_reviewer",
  model = "anthropic-cli/sonnet",
  actions = { "git.diff" },
})
```

**Required permission:** `ai:anthropic-cli`

```toml
[runner.local_reviewer]
allowed_actions = ["git.diff"]
granted = ["ai:anthropic-cli"]
```

## openai-cli

Uses the locally installed `codex` CLI as a text-output fallback. This backend invokes `codex` and captures its text output.

Use `openai-cli` for text-only `ctx.ai` calls. It cannot call agent.d actions.

```lua
agentd.tool({
  name = "assist",
  requires = { "ai:openai-cli" },
})

agentd.action({
  name = "assist.ask",
  requires = { "ai:openai-cli" },
  handler = function(args, ctx)
    return ctx.ai.ask(args.prompt, {
      model = "openai-cli/",
    })
  end,
})
```

**Required permission:** `ai:openai-cli`

```toml
[tool.assist]
granted = ["ai:openai-cli"]
```

::: warning Requires local CLI on PATH
Both backends require the respective CLI (`claude` or `codex`) to be installed and on `PATH` where the `agentd` process runs. On Windows, native executables and common launcher shims (`.ps1`, `.cmd`, and `.bat`) are resolved automatically. If the program is not found, the call will fail at runtime.
:::

## See also

- [Providers overview](/v0/providers/) — all registered prefixes and model selection
- [Credentials](/v0/providers/credentials) — keyring-based credentials for the direct API backends
- [Anthropic provider](/v0/providers/anthropic) — direct Anthropic Messages API backend

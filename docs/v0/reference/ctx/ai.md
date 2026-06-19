# ctx.ai — Model Calls

`ctx.ai` makes calls to language models through any registered provider. Use it when you need an inline model response without spinning up a full runner.

**Required permission:** `ai:<provider>` — e.g. `ai:anthropic` or `ai:*`.

## Signatures

```lua
ctx.ai.ask(prompt: string, opts?: {
  provider?:   string,
  model?:      string,
  max_tokens?: integer,
  system?:     string,
}) -> string

ctx.ai.complete({
  model?:      string,
  system?:     string,
  prompt:      string,
  max_tokens?: integer,
  messages?:   { role: string, content: string }[],
}) -> table

ctx.ai.providers() -> string[]
```

## Methods

### `ctx.ai.ask`

A convenience wrapper that sends `prompt` to a model and returns the response as a plain string. Ideal for single-turn summarization, classification, or extraction tasks.

| Parameter | Type | Description |
|---|---|---|
| `prompt` | `string` | The user prompt. |
| `provider` | `string` | Provider prefix (e.g. `"anthropic"`, `"openai"`). Defaults to `"anthropic"`. |
| `model` | `string` | Full model string `"<provider>/<model_id>"` or just `"<model_id>"`. |
| `max_tokens` | `integer` | Maximum tokens in the response. |
| `system` | `string` | System prompt to prepend. |

**Returns:** `string` — the model's text response.

### `ctx.ai.complete`

Lower-level call that accepts a full message list and returns the raw response table from the provider.

| Parameter | Type | Description |
|---|---|---|
| `model` | `string` | Model string. |
| `system` | `string` | System prompt. |
| `prompt` | `string` | User prompt (required). |
| `max_tokens` | `integer` | Maximum response tokens. |
| `messages` | `table[]` | Prior conversation turns as `{ role, content }` pairs. |

**Returns:** `table` — the raw provider response.

### `ctx.ai.providers`

Returns the list of available provider prefixes or `"<prefix>/<model_id>"` strings configured in the daemon.

**Returns:** `string[]`.

## Model string format

Models are identified as `"<provider>/<model_id>"`. Registered provider prefixes:

| Prefix | Backend |
|---|---|
| `anthropic` | Anthropic Messages API |
| `anthropic-cli` | Local `claude` CLI |
| `openai` | OpenAI-compatible API |
| `codex` | `codex app-server` JSON-RPC |
| `openai-cli` | Local `codex` CLI |

When the prefix is omitted, `anthropic` is the default.

See [Providers](/v0/providers/) for credentials and setup.

## Permission

```toml
[tool.summarizer]
granted = ["ai:anthropic"]
```

A wildcard `ai:*` permits calls to any provider.

## Examples

```lua
-- Summarize a file with a one-liner
agentd.action("doc.summarize", function(args, ctx)
  local content = ctx.fs.read(args.path)
  return ctx.ai.ask("Summarize this document in 3 bullet points:\n\n" .. content, {
    model      = "anthropic/claude-opus-4-7",
    max_tokens = 256,
  })
end)
```

```lua
-- List available providers at runtime
agentd.action("ai.providers", function(args, ctx)
  return ctx.ai.providers()
end)
```

```lua
-- Multi-turn conversation with ctx.ai.complete
agentd.action("chat.reply", function(args, ctx)
  local result = ctx.ai.complete({
    model    = "openai/gpt-5.5",
    system   = "You are a helpful assistant.",
    prompt   = args.message,
    messages = args.history or {},
  })
  return result
end)
```

## See also

- [ctx.call — runners](/v0/reference/ctx/calls)
- [Providers](/v0/providers/)
- [Concepts: runners](/v0/concepts/runners)
- [Security: permission slugs](/v0/security/permission-slugs)

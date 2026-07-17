# Custom providers

Register any **OpenAI-compatible** endpoint (OpenRouter, Groq, Together, vLLM, Ollama, LM Studio, …) or **Anthropic-compatible** gateway as a named provider in `config.toml`. No code changes, no plugin — the daemon builds the provider at startup and it behaves exactly like the built-ins: same Lua API, same tool calling, same schemas, same `ctx.structured`.

## Declaring a provider

Each `[providers.<name>]` table adds one prefix to the registry:

```toml
# config.toml
[providers.openrouter]
kind = "openai"                       # wire format: "openai" | "anthropic"
base_url = "https://openrouter.ai/api/v1"
api_key_secret = "openrouter_api_key" # secret store key holding the API key
default_model = "meta-llama/llama-3.3-70b-instruct"

[providers.groq]
kind = "openai"
base_url = "https://api.groq.com/openai/v1"
api_key_secret = "groq_api_key"

[providers.ollama]
kind = "openai"
base_url = "http://localhost:11434/v1"
auth = "none"                         # local server, no Authorization header
default_model = "qwen3:14b"
```

- **`kind`** — `"openai"` for the Chat Completions wire format, `"anthropic"` for the Messages wire format (self-hosted gateways/proxies).
- **`base_url`** — forgiving: `https://host/v1`, with or without a trailing slash, or the full `/chat/completions` (or `/v1/messages`) URL all work.
- **`api_key_secret`** — name of the key in the OS keyring (see [Credentials](/v0/providers/credentials)). Exactly one of `api_key_secret` or `auth = "none"` is required; a remote endpoint without credentials must be opted into explicitly.
- **`auth = "none"`** — send no auth header at all. For local servers like Ollama or vLLM without `--api-key`.
- **`default_model`** — used when a call passes no model id.

Provider names must not collide with the reserved built-in prefixes (`anthropic`, `anthropic-cli`, `openai`, `codex`, `openai-cli`, `mock`).

## Storing the API key

Same pattern as the built-ins — once, with `agentctl secret set` under the name you declared in `api_key_secret`:

```bash
echo "$OPENROUTER_API_KEY" | agentctl secret set openrouter_api_key
```

## Using the provider

The new prefix works everywhere a built-in does. Model ids may contain slashes:

```lua
agentd.runner({
  name = "summariser",
  model = "openrouter/meta-llama/llama-3.3-70b-instruct",
  skills = { "summarise" },
})
```

```lua
local reply = ctx.ai.ask("Summarise this PR", { model = "ollama/qwen3:14b" })
```

Switching providers changes nothing else — tool calling, input/output schemas, and `ctx.structured` behave identically across all of them.

## Default provider

Point bare model ids (no `provider/` prefix) at your provider:

```toml
[runtime]
default_provider = "ollama"
```

## Required permission

As with built-ins, callers need the matching grant:

```toml
# grants.toml
[service.local_bot]
granted = ["ai:ollama"]
```

## See also

- [Providers overview](/v0/providers/) — model selection, max_turns, all prefixes
- [Credentials](/v0/providers/credentials) — storing and rotating API keys
- [OpenAI](/v0/providers/openai) — the built-in `openai` prefix

# Custom providers

A custom provider connects agent.d to a model API that is not a built-in provider.

The API must use one of these supported formats:

| `kind` value | Required API format |
|---|---|
| `openai` | OpenAI Chat Completions |
| `anthropic` | Anthropic Messages |

agent.d does not install or start the model server. Before you make a model call, make sure that the API endpoint is available.

## Add a local provider

Add a provider table to `~/.config/agentd/config.toml`:

```toml
[providers.ollama]
kind = "openai"
base_url = "http://127.0.0.1:11434/v1"
auth = "none"
default_model = "qwen3:14b"
```

This example registers the provider name `ollama`. It assumes that an OpenAI-compatible server listens on port `11434`.

Replace the URL and model ID when your server uses different values.

The provider table has these fields:

| Field | Use this field when | Function |
|---|---|---|
| `kind` | For every provider | Selects the supported API format. |
| `base_url` | For every provider | Specifies the model API endpoint. |
| `auth` | The endpoint has no authentication | Disables the authentication header when its value is `none`. |
| `api_key_secret` | The endpoint uses an API key | Specifies the secret-store name that contains the API key. |
| `default_model` | You want to specify a fallback model | Specifies the model when a call does not specify one. |

Every provider must contain exactly one of these authentication fields: `auth` or `api_key_secret`. Use `auth = "none"` only when the endpoint does not require authentication.

For an OpenAI-compatible API, `base_url` can contain the API base path or the full Chat Completions path.

For an Anthropic-compatible API, `base_url` can contain the API base path or the full Messages path.

## Use the provider in a runner

Add a runner to `init.lua` or to a Lua file that `init.lua` loads:

```lua
agentd.runner({
    name = "local_helper",
    model = "ollama/qwen3:14b",
})
```

The text before the first `/` is the provider name. The remaining text is the model ID.

A model ID can contain `/` characters after the first separator. agent.d keeps these characters in the model ID.

Give the runner permission to call the provider:

```toml
[runner.local_helper]
granted = ["ai:ollama"]
```

The grant must contain the provider name. A grant for a different provider does not permit this call.

## Add an authenticated provider

Use `api_key_secret` when the endpoint requires an API key:

```toml
[providers.gateway]
kind = "anthropic"
base_url = "https://gateway.example.com/v1"
api_key_secret = "gateway_api_key"
default_model = "claude-compatible-model"
```

The `api_key_secret` value is a secret-store name. It is not the API key.

Store the API key under that name. Refer to [Provider credentials](/v0/providers/credentials) for the applicable `agentctl` commands.

The provider gets the key from the secret store when it makes a model call. You do not need to restart agent.d after a key change.

## Select a default provider

You can select a custom provider as the default provider:

```toml
[runtime]
default_provider = "ollama"
```

After this change, a model string without a provider prefix uses `ollama`.

For example, `model = "qwen3:14b"` uses the `ollama` provider. The model string `openai/gpt-5.5` still selects `openai`.

## Provider-name restrictions

A custom provider name cannot use a built-in provider name.

These names are reserved:

- `anthropic`
- `anthropic-cli`
- `openai`
- `codex`
- `openai-cli`
- `mock`

The daemon rejects an invalid provider configuration at startup. The error identifies the provider and the invalid field.

The daemon also rejects these configurations:

- `base_url` is empty.
- Both authentication fields are present.
- Both authentication fields are absent.
- `auth` has a value other than `none`.
- `runtime.default_provider` identifies an unknown provider.

## Endpoint compatibility

The endpoint must implement the selected API format. Some compatible endpoints do not implement every optional feature.

Confirm that your endpoint supports the model, message, and tool features that your runner uses.

## See also

- [Providers overview](/v0/providers/)
- [Provider credentials](/v0/providers/credentials)
- [Configuration reference](/v0/reference/configuration)
- [Permission slugs](/v0/security/permission-slugs)

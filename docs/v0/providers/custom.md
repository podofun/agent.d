# Custom Providers

A custom provider sends model requests to an API endpoint that you select, using one of the two API formats that agent.d supports.

| `kind` value | API format |
|---|---|
| `openai` | OpenAI Chat Completions |
| `anthropic` | Anthropic Messages |

agent.d does not install or start the API endpoint, so make sure that the endpoint is available before you make a model call.

## Add a provider without authentication

Add a provider table to `~/.config/agentd/config.toml`:

```toml
[providers.ollama]
kind = "openai"
base_url = "http://127.0.0.1:11434/v1"
auth = "none"
default_model = "qwen3:14b"
```

This table registers `ollama` as the provider name and tells agent.d not to send an authentication header.

Use `auth = "none"` only when the endpoint does not require authentication.

## Set the provider fields

Use these fields to configure each provider table:

| Field | Requirement | Purpose |
|---|---|---|
| `kind` | Required | Selects the API format. |
| `base_url` | Required | Sets the API base URL or complete endpoint URL. |
| `auth` | Use for an endpoint without authentication. | Turns off the authentication header when the value is `none`. |
| `api_key_secret` | Use for an endpoint that requires an API key. | Identifies the keyring secret that contains the API key. |
| `default_model` | Optional | Sets the model when a call does not supply one. |

Set exactly one of `auth` and `api_key_secret` because agent.d rejects a provider table that contains both fields or neither field.

If you omit `default_model`, agent.d uses `gpt-4.1` for OpenAI format or `claude-opus-4-7` for Anthropic format.

## Set an OpenAI endpoint URL

For `kind = "openai"`, agent.d uses the URL without a change when it ends with `/chat/completions`.
For all other URLs, agent.d adds `/chat/completions`.

This example uses the following OpenAI base URL:

```text
http://127.0.0.1:11434/v1
```

agent.d sends the model request to this URL:

```text
http://127.0.0.1:11434/v1/chat/completions
```

## Set an Anthropic endpoint URL

For `kind = "anthropic"`, agent.d uses the URL without a change when it ends with `/v1/messages`.
If the URL ends with `/v1`, agent.d adds `/messages`.
For all other URLs, agent.d adds `/v1/messages`.

This example uses the following Anthropic base URL:

```text
https://gateway.example.com/v1
```

agent.d sends the model request to this URL:

```text
https://gateway.example.com/v1/messages
```

## Add a provider with an API key

Use `api_key_secret` when the endpoint requires an API key:

```toml
[providers.gateway]
kind = "anthropic"
base_url = "https://gateway.example.com/v1"
api_key_secret = "gateway_api_key"
default_model = "claude-compatible-model"
```

The `api_key_secret` value is the keyring name, not the API key.

Store the key with the same name:

```bash
echo "$GATEWAY_API_KEY" | agentctl secret set gateway_api_key
```

The provider reads the secret when each model call starts, so a key change does not require an agent.d restart.

## Use the provider with a runner

To select a custom provider for a runner, use `<provider>/<model-id>`:

```lua
agentd.runner({
    name = "local_helper",
    model = "ollama/qwen3:14b",
})
```

When the endpoint supports action calls, the provider can return them and agent.d checks each call in the action loop.

## Use the provider with `ctx.ai`

Use the custom provider name in the model string for a direct `ctx.ai` call.
The following direct call does not give agent.d actions to the provider:

```lua
local reply = ctx.ai.ask("Summarize this text.", {
    model = "ollama/qwen3:14b",
})
```

Give the caller the permission for the custom provider name:

```toml
[tool.local_assist]
granted = ["ai:ollama"]
```

A permission for a different provider does not permit this call.

## Select the default provider

Use `runtime.default_provider` to select a custom provider as the default:

```toml
[runtime]
default_provider = "ollama"
```

After this change, a model string without `/` uses `ollama`.

```lua
ctx.ai.ask("Summarize this text.", {
    model = "qwen3:14b",
})
```

Always include the provider name when the model ID contains `/`.

## Select a provider name

Use a nonempty provider name without `/` because a model string cannot select a provider name that contains `/`.

A custom provider cannot use one of these reserved names:

- `anthropic`
- `anthropic-cli`
- `openai`
- `codex`
- `openai-cli`
- `mock`

The `mock` name is reserved for tests, and this provider is not available in a normal agent.d session.
agent.d accepts `mock` as the default at startup, but it cannot complete a model call with this provider.

## Correct configuration errors

agent.d rejects these provider configurations at startup:

- The provider table does not contain `kind` or `base_url`.
- The provider table contains an unknown field.
- `base_url` is empty.
- The provider name matches a reserved name.
- Both authentication fields are present.
- Both authentication fields are absent.
- `auth` has a value other than `none`.
- `runtime.default_provider` identifies an unknown provider.

The startup error identifies the applicable provider or field.

## Check endpoint compatibility

The endpoint must implement the selected API format and support each model, message type, and action feature that your call uses.

## See also

- [Providers](/v0/providers/)
- [Provider credentials](/v0/providers/credentials)
- [Configuration reference](/v0/reference/configuration)
- [Permission slugs](/v0/security/permission-slugs)

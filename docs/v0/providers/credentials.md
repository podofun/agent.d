# Provider Credentials

API providers that require credentials read their keys from the OS keyring.
An API provider with `auth = "none"` does not use a key.
The CLI providers use the authentication data of their terminal applications.

| Provider type | Credential source |
|---|---|
| `anthropic` | The `anthropic_api_key` secret |
| `openai` | The `openai_api_key` secret |
| Custom API provider | The secret in `api_key_secret`, or no credential with `auth = "none"` |
| `anthropic-cli` | Claude Code authentication |
| `codex` | Codex authentication |
| `openai-cli` | Codex authentication |

## Store a key

Use `agentctl secret set` to store a key, and send the value through standard input instead of the command line.

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

The command prints this message after it stores the key:

```text
stored `anthropic_api_key` — available to the daemon immediately
```

`agentctl` writes to the same keyring that agent.d uses, and agent.d does not have to be active when you run the command.

Each API provider reads its key when a model call starts, so a key change does not require an agent.d restart.

::: warning Do not put a key on the command line
Your shell can save command-line values in its history.
Do not put a key in a Lua file or `config.toml`.
Do not commit a key to a repository.
:::

## Check a key

Use `agentctl secret peek` to show a masked part of a key:

```bash
agentctl secret peek anthropic_api_key
```

`agentctl` shows the stored key in this masked form:

```text
sk-a************yz (24 chars)
```

The command does not print the complete key, and it fails if the key does not exist.

## Remove a key

Use `agentctl secret unset` to remove a key:

```bash
agentctl secret unset anthropic_api_key
```

After `agentctl` removes the key, it prints this confirmation:

```text
removed `anthropic_api_key`
```

## Replace a key

Use `agentctl secret set` with the new value to replace the previous value.

```bash
echo "$NEW_ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

The next model call uses the new value.

## Store a key for a custom provider

The `api_key_secret` field contains the keyring name, not the API key.

The following provider reads the `gateway_api_key` secret:

```toml
[providers.gateway]
kind = "anthropic"
base_url = "https://gateway.example.com/v1"
api_key_secret = "gateway_api_key"
```

Store the key with the same name:

```bash
echo "$GATEWAY_API_KEY" | agentctl secret set gateway_api_key
```

## Use secrets from Lua

Provider setup does not require Lua code, so use `ctx.secret` only when Lua code must manage a secret.

| Call | Result |
|---|---|
| `ctx.secret.get(key)` | Returns the value. The call fails if the key does not exist. |
| `ctx.secret.set(key, value)` | Stores or replaces the value. |
| `ctx.secret.exists(key)` | Returns `true` if the key exists. |
| `ctx.secret.delete(key)` | Removes the key. The call fails if the key does not exist. |
| `ctx.secret.list()` | Returns secret names that this agent.d process stored through `ctx.secret.set`. |

`ctx.secret.list()` returns only secret names that the current agent.d process stored through `ctx.secret.set`.
The list does not include secret names that `agentctl` or an earlier agent.d process stored.

## Give permissions to Lua code

The key-specific calls require the `secret:<key>` permission, and the `ctx.secret.list()` call requires the `secret:*` permission.

```toml
[tool.discord]
granted = ["secret:discord_token"]
```

The `agentctl secret` commands do not require an agent.d permission.

An authenticated API provider reads its credential from the OS keyring without giving the credential to Lua code.
A `ctx.ai` caller needs the applicable `ai:<provider>` permission.
Give the caller a `secret:<key>` permission only if the Lua code must access the credential through `ctx.secret`.

## See also

- [`agentctl secret`](/v0/reference/cli#agentctl-secret-set)
- [ctx.secret](/v0/reference/ctx/secrets)
- [Custom providers](/v0/providers/custom)
- [Permission slugs](/v0/security/permission-slugs)

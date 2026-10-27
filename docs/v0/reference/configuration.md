# Configuration Reference

This page lists every setting of the `agentd` daemon: its command-line flags, environment variables and `config.toml` fields.

## Resolution precedence

When a setting is given in more than one place, the daemon uses the first source in this order:

```
CLI flag  >  environment variable  >  config.toml  >  built-in default
```

`log_level` also reads `RUST_LOG`, between `config.toml` and the built-in default, so a development setup that already exports `RUST_LOG` keeps working.

---

## Flag and environment variable reference

| Flag | Short | Env | Default | Description |
|---|---|---|---|---|
| `--config <path>` | `-c` | `AGENTD_CONFIG` | `$XDG_CONFIG_HOME/agentd/config.toml` | Path to `config.toml` |
| `--init <path>` | `-i` | `AGENTD_INIT` | `$XDG_CONFIG_HOME/agentd/init.lua` | Lua entry point |
| `--grants <path>` | `-g` | `AGENTD_GRANTS` | `$XDG_CONFIG_HOME/agentd/grants.toml` | grants.toml path |
| `--addr <addr>` | `-a` | `AGENTD_ADDR` | `127.0.0.1:7777` | HTTP + WebSocket bind address |
| `--trace <path>` | | `AGENTD_TRACE` | `$XDG_STATE_HOME/agentd/trace.jsonl` | JSONL trace sink |
| `--log <filter>` | `-l` | `AGENTD_LOG` | `warn` | Log filter, for example `info` or `debug` |
| `--token <s>` | `-t` | `AGENTD_TOKEN` | auto-minted | `/ws` bearer token |
| `--admin-token <s>` | | `AGENTD_ADMIN_TOKEN` | auto-minted | `/control` bearer token |
| `--no-auth` | | `AGENTD_NO_AUTH` | `false` | Disable `/ws` and `/control` auth. Every `ws` client becomes an [admin connection](/v0/security/approvals#admin-connections) |
| `--approval-timeout <n>` | | `AGENTD_APPROVAL_TIMEOUT_MS` | `120000` | Approval wait budget (ms) |
| `--secrets <backend>` | | `AGENTD_SECRETS` | `keyring` | Secrets backend: `keyring`, `env`, or `dir:<path>` |
| `--watch` | `-w` | `AGENTD_WATCH` | `false` | Dev hot reload |
| `--install-sandbox` | | | | Windows only: one-time setup for sandboxed networking, then exit. See [Shell sandbox](/v0/security/sandbox#one-time-setup) |
| `--uninstall-sandbox` | | | | Reverse `--install-sandbox` (macOS/Windows), then exit |

::: info Deprecated aliases
The older flags `--grants-file`, `--trace-file` and `--approval-timeout-ms` still work as hidden aliases of `--grants`, `--trace` and `--approval-timeout` for one more release, and will then be removed. The environment variables `AGENTD_GRANTS_FILE` and `AGENTD_TRACE_FILE` are deprecated aliases of `AGENTD_GRANTS` and `AGENTD_TRACE`.
:::

---

## config.toml

When `--config` is not given, the daemon reads `$XDG_CONFIG_HOME/agentd/config.toml`. If the file is missing, every field uses its default. If the file cannot be parsed, the daemon stops with an error.

### `[daemon]` section

```toml
[daemon]
addr                = "127.0.0.1:7777"
trace_file          = "~/.local/state/agentd/trace.jsonl"
log_level           = "warn"
# token             = "..."          # set explicitly or leave unset to auto-mint
no_auth             = false
# admin_token       = "..."          # set explicitly or leave unset to auto-mint
approval_timeout_ms = 120000
# secrets           = "keyring"      # or "env", or "dir:/run/secrets"
```

| Field | Type | Default | Description |
|---|---|---|---|
| `addr` | string | `"127.0.0.1:7777"` | Bind address for HTTP and WebSocket |
| `trace_file` | string | `~/.local/state/agentd/trace.jsonl` | JSONL trace output path; `~/` is expanded |
| `log_level` | string | `"warn"` | Log filter; falls back to `RUST_LOG` |
| `token` | string | | Fixed `/ws` bearer token; leave it out to have one generated |
| `no_auth` | bool | `false` | Disable authentication on both `/ws` and `/control` |
| `admin_token` | string | | Fixed `/control` bearer token; leave it out to have one generated |
| `approval_timeout_ms` | integer | `120000` | How long (ms) to wait for an operator's answer before denying the request |
| `secrets` | string | `"keyring"` | Secrets backend: `keyring` (OS keyring), `env` (`AGENTD_SECRET_<NAME>` variables), or `dir:<path>` (one file per secret, for example Docker or Kubernetes secret mounts). `env` and `dir` are read-only |

### `[runtime]` section

```toml
[runtime]
init      = "~/.config/agentd/init.lua"
max_turns = 16
yolo      = false   # RESERVED — emits a warning and has no other effect
# watch            = false
# default_provider = "anthropic"
```

| Field | Type | Default | Description |
|---|---|---|---|
| `init` | string | `~/.config/agentd/init.lua` | Lua entry point path; `~/` is expanded |
| `max_turns` | integer | `16` | Maximum tool-use loop iterations per runner call outside a session. Session runs have no limit |
| `yolo` | bool | `false` | **Reserved.** Currently emits a startup warning and is otherwise ignored |
| `watch` | bool | `false` | Enable dev hot reload (same as `--watch` flag) |
| `default_provider` | string | `anthropic` | Provider used when a model string has no `provider/` prefix. Any built-in or `[providers.<name>]` entry |

### `[providers.<name>]` sections

Each `[providers.<name>]` table adds a model provider under the prefix `<name>`. It can point at any OpenAI-compatible endpoint, such as OpenRouter, Groq, Together, vLLM, Ollama, LM Studio or GitHub Models, or at an Anthropic-compatible gateway:

```toml
[providers.openrouter]
kind = "openai"
base_url = "https://openrouter.ai/api/v1"
api_key_secret = "openrouter_api_key"
default_model = "meta-llama/llama-3.3-70b-instruct"

[providers.ollama]
kind = "openai"
base_url = "http://localhost:11434/v1"
auth = "none"
default_model = "qwen3:14b"
```

| Field | Type | Required | Description |
|---|---|---|---|
| `kind` | `"openai"` \| `"anthropic"` | yes | Which API shape the endpoint speaks |
| `base_url` | string | yes | Endpoint base; with or without the trailing API path, both work |
| `api_key_secret` | string | one of these two | Secret-store key holding the API key (see [Credentials](/v0/providers/credentials)) |
| `auth` | `"none"` | one of these two | Send no auth header, for local servers without authentication |
| `default_model` | string | no | Model used when a call passes no model id |

A name cannot reuse a built-in prefix: `anthropic`, `anthropic-cli`, `openai`, `codex`, `openai-cli` or `mock`. Each entry needs exactly one of `api_key_secret` or `auth = "none"`, so connecting to a host without credentials is a choice you make on purpose. If a `[providers]` entry is invalid, the daemon does not start, and the error names the provider.

See [Custom providers](/v0/providers/custom) for a usage-focused walkthrough.

### `[webhooks.<name>]` sections

Use a webhook route to let another service, such as GitHub or a payment provider, call one of your actions over HTTP. Each entry creates `POST /webhooks/<name>`, which checks the request's HMAC-SHA256 signature and then calls one action.

```toml
[webhooks.events]
action = "events.ingest"
secret = "webhook_secret"
signature_header = "x-webhook-signature-256"
signature_prefix = "sha256="
id_header = "x-webhook-id"
```

| Field | Type | Required | Description |
|---|---|---|---|
| `action` | string | yes | Action to call after verification |
| `secret` | string | yes | Keyring name for the HMAC secret |
| `signature_header` | string | yes | Header that contains the hexadecimal signature |
| `signature_prefix` | string | no | Prefix before the signature; the default is empty |
| `id_header` | string | no | Header to use for `ctx.caller.session` |

Store the secret before starting the daemon:

```bash
echo "$WEBHOOK_SECRET" | agentctl secret set webhook_secret
```

Webhook names can contain letters, numbers, hyphens and underscores. At startup, agent.d checks each route, its target action and its secret, and does not start if any of them is invalid.

The route computes the HMAC-SHA256 value over the exact request body. Every request must carry a valid signature. The action receives `headers` and `payload`. Header names are lowercase, and the authentication and signature headers are left out.

The action runs with the caller interface `webhook.<name>`, and its grants are checked like any other call. The request ID becomes `ctx.caller.session`. When `id_header` is set, each request must include that header, and its value is the request ID. Without `id_header`, agent.d generates an ID.

Restart agent.d after you change a route or its secret.

See [Receive a signed webhook](/v0/recipes/webhook) for the setup procedure.

---

### `[interfaces.<name>]` sections

Give each client its own token so the daemon can tell clients apart. You can then give a web backend, a Telegram bridge and a Discord bot separate `[interface.<name>]` grants in `grants.toml`, and each client sees only the chat sessions its interface owns.

```toml
[interfaces.webapp]
token_secret = "webapp_ws_token"   # read from the secrets backend

[interfaces.telegram]
token = "a-literal-token"          # fine for local development
```

| Field | Description |
|---|---|
| `token` | Literal bearer token the client presents on `/ws`. |
| `token_secret` | Name of a secret holding the token. Each entry needs exactly one of `token` or `token_secret`. |

When a client connects with an interface token, `ctx.caller.interface` reports its name. A connection presenting the daemon token (`daemon.token`) is interface `ws`, so `ws` is reserved and cannot be an interface name. The daemon refuses to start if two interfaces share a token or an interface uses the daemon token.

## Auto-minted tokens

When authentication is on and you have not set a token, the daemon generates random tokens at startup. It saves them in the state directory with mode `0600`, where `agentctl` on the same machine finds them:

| File | Used by |
|---|---|
| `$XDG_STATE_HOME/agentd/token` | `/ws` bearer token |
| `$XDG_STATE_HOME/agentd/admin-token` | `/control` bearer token |

---

## `--watch` hot reload

Hot reload applies your changes to Lua files, skills and grants while the daemon keeps running, which is what you want during development. Turn it on with `--watch`, `AGENTD_WATCH=1` or `watch = true` in `[runtime]`. The daemon then watches:

- `init.lua` and every file loaded via `import()`
- All skill `.md` files loaded via `agentd.skills.load()` or `agentd.skills.dir()`
- `grants.toml`

When one of these files changes, the daemon rebuilds the Lua runtime. Requests already in progress finish on the old runtime, and new requests use the new one. After a reload, the daemon also regenerates the `.luals/` type stubs, as `agentctl types` does.

Durable memory (`memory.redb`) and any connected approval operator on `/control` survive reloads.

::: tip Dev workflow
Run `agentd --watch` during development to pick up edits to Lua files and skills without a restart.
:::

---

## Directory locations

The daemon keeps its files in three per-user base directories, config, state and data, placed where each operating system expects them. You can override any single path with the matching flag or environment variable from the tables above.

| Base | Holds | Linux | macOS | Windows |
|---|---|---|---|---|
| **Config** | `config.toml`, `init.lua`, `grants.toml` | `$XDG_CONFIG_HOME/agentd/` (`~/.config/agentd/`) | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |
| **State** | `token`, `admin-token`, `trace.jsonl` | `$XDG_STATE_HOME/agentd/` (`~/.local/state/agentd/`) | `~/Library/Application Support/agentd/` | `%LOCALAPPDATA%\agentd\` |
| **Data** | `packages/`, `memory.redb` | `$XDG_DATA_HOME/agentd/` (`~/.local/share/agentd/`) | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |

On Linux, `$XDG_CONFIG_HOME` defaults to `~/.config`, `$XDG_STATE_HOME` to `~/.local/state` and `$XDG_DATA_HOME` to `~/.local/share`. If `$XDG_STATE_HOME` is unset, the state directory falls back to the local data directory. Examples in these docs use the Linux paths; on macOS or Windows, use the matching column.

---

## See also

- [Deployment](/v0/operations/deployment)
- [agentctl CLI](/v0/reference/cli)
- [Permissions & grants](/v0/security/grants)
- [Observability](/v0/operations/observability)

# Configuration Reference

This page covers every knob you can turn to configure the `agentd` binary: command-line flags, environment variables, and `config.toml` fields.

## Resolution precedence

For every configuration knob the daemon applies this priority order — highest wins:

```
CLI flag  >  environment variable  >  config.toml  >  built-in default
```

`log_level` has one additional fallback between `config.toml` and the built-in default: `RUST_LOG`. This lets existing dev workflows that already export `RUST_LOG` keep working without touching `config.toml`.

---

## Flag and environment variable reference

| Flag | Short | Env | Default | Description |
|---|---|---|---|---|
| `--config <path>` | `-c` | `AGENTD_CONFIG` | `$XDG_CONFIG_HOME/agentd/config.toml` | Path to `config.toml` |
| `--init <path>` | `-i` | `AGENTD_INIT` | `$XDG_CONFIG_HOME/agentd/init.lua` | Lua entry point |
| `--grants <path>` | `-g` | `AGENTD_GRANTS` | `$XDG_CONFIG_HOME/agentd/grants.toml` | grants.toml path |
| `--addr <addr>` | `-a` | `AGENTD_ADDR` | `127.0.0.1:7777` | HTTP + WebSocket bind address |
| `--trace <path>` | — | `AGENTD_TRACE` | `$XDG_STATE_HOME/agentd/trace.jsonl` | JSONL trace sink |
| `--log <filter>` | `-l` | `AGENTD_LOG` | `warn` | tracing-subscriber filter string |
| `--token <s>` | `-t` | `AGENTD_TOKEN` | auto-minted | `/ws` bearer token |
| `--admin-token <s>` | — | `AGENTD_ADMIN_TOKEN` | auto-minted | `/control` bearer token |
| `--no-auth` | — | `AGENTD_NO_AUTH` | `false` | Disable `/ws` and `/control` auth |
| `--approval-timeout <n>` | — | `AGENTD_APPROVAL_TIMEOUT_MS` | `120000` | Approval wait budget (ms) |
| `--watch` | `-w` | `AGENTD_WATCH` | `false` | Dev hot reload |
| `--install-sandbox` | — | — | — | Windows only: one-time setup for sandboxed networking, then exit. See [Shell sandbox](/v0/security/sandbox#windows-one-time-network-setup) |
| `--uninstall-sandbox` | — | — | — | Reverse `--install-sandbox` (macOS/Windows), then exit |

::: info Deprecated aliases
The pre-rename long flags `--grants-file`, `--trace-file`, and `--approval-timeout-ms` still work as hidden aliases of `--grants`, `--trace`, and `--approval-timeout` for one release, after which they will be removed. Likewise the env vars `AGENTD_GRANTS_FILE` and `AGENTD_TRACE_FILE` remain deprecated aliases of `AGENTD_GRANTS` and `AGENTD_TRACE`.
:::

---

## config.toml

When `--config` is not given, the daemon reads `$XDG_CONFIG_HOME/agentd/config.toml`. A missing file is treated as an empty file (all fields fall through to defaults). A malformed file is a hard error.

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
```

| Field | Type | Default | Description |
|---|---|---|---|
| `addr` | string | `"127.0.0.1:7777"` | Bind address for HTTP and WebSocket |
| `trace_file` | string | `~/.local/state/agentd/trace.jsonl` | JSONL trace output path; `~/` is expanded |
| `log_level` | string | `"warn"` | tracing-subscriber filter; also falls back to `RUST_LOG` |
| `token` | string | — | Fixed `/ws` bearer token; omit to auto-mint |
| `no_auth` | bool | `false` | Disable authentication on both `/ws` and `/control` |
| `admin_token` | string | — | Fixed `/control` bearer token; omit to auto-mint |
| `approval_timeout_ms` | integer | `120000` | How long (ms) to wait for an operator verdict before failing closed |

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
| `max_turns` | integer | `16` | Maximum tool-use loop iterations per runner call |
| `yolo` | bool | `false` | **Reserved.** Currently emits a startup warning and is otherwise ignored |
| `watch` | bool | `false` | Enable dev hot reload (same as `--watch` flag) |
| `default_provider` | string | `anthropic` | Provider used when a model string has no `provider/` prefix. Any built-in or `[providers.<name>]` entry |

### `[providers.<name>]` sections

Each `[providers.<name>]` table registers an extra model provider under the
prefix `<name>` — any OpenAI-compatible endpoint (OpenRouter, Groq, Together,
vLLM, Ollama, LM Studio, GitHub Models, …) or Anthropic-compatible gateway:

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
| `auth` | `"none"` | one of these two | Send no auth header — for local servers without authentication |
| `default_model` | string | no | Model used when a call passes no model id |

Names must not collide with the reserved built-in prefixes (`anthropic`,
`anthropic-cli`, `openai`, `codex`, `openai-cli`, `mock`). Exactly one of
`api_key_secret` or `auth = "none"` is required — pointing at a remote host
without credentials must be an explicit choice. The daemon refuses to start on
any invalid `[providers]` entry and names the offending provider in the error.

See [Custom providers](/v0/providers/custom) for a usage-focused walkthrough.

---

## Auto-minted tokens

When auth is enabled and no explicit token is configured, the daemon generates random tokens at startup and persists them to the state directory (mode `0600`) so that local `agentctl` can find them automatically:

| File | Used by |
|---|---|
| `$XDG_STATE_HOME/agentd/token` | `/ws` bearer token |
| `$XDG_STATE_HOME/agentd/admin-token` | `/control` bearer token |

---

## `--watch` hot reload

`--watch` (or `AGENTD_WATCH=1` / `watch = true` in `[runtime]`) enables dev hot reload. When active the daemon watches:

- `init.lua` and every file loaded via `import()`
- All skill `.md` files loaded via `agentd.skills.load()` or `agentd.skills.dir()`
- `grants.toml`

On any change it rebuilds the Lua runtime in place. In-flight requests drain on the old runtime (the executor is swapped via `ArcSwap` so ongoing requests keep their reference until they complete). After reload it also regenerates `.luals/` type stubs, identical to running `agentctl types`.

Durable memory (`memory.redb`) and any connected approval operator on `/control` survive reloads.

::: tip Dev workflow
Run `agentd --watch` during development so you never need to restart after editing Lua files or skills.
:::

---

## Directory locations

The daemon resolves three per-user base directories — **config**, **state**,
and **data** — using the platform-native convention for each OS. Any individual path can be
overridden with the matching flag or environment variable in the tables above.

| Base | Holds | Linux | macOS | Windows |
|---|---|---|---|---|
| **Config** | `config.toml`, `init.lua`, `grants.toml` | `$XDG_CONFIG_HOME/agentd/` (`~/.config/agentd/`) | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |
| **State** | `token`, `admin-token`, `trace.jsonl` | `$XDG_STATE_HOME/agentd/` (`~/.local/state/agentd/`) | `~/Library/Application Support/agentd/` | `%LOCALAPPDATA%\agentd\` |
| **Data** | `packages/`, `memory.redb` | `$XDG_DATA_HOME/agentd/` (`~/.local/share/agentd/`) | `~/Library/Application Support/agentd/` | `%APPDATA%\agentd\` |

On Linux, `$XDG_CONFIG_HOME` defaults to `~/.config`, `$XDG_STATE_HOME` to
`~/.local/state`, and `$XDG_DATA_HOME` to `~/.local/share`; if `$XDG_STATE_HOME`
is unset the state directory falls back to the local data directory. Examples
throughout these docs use the Linux/XDG form — substitute the matching column
on macOS or Windows.

---

## See also

- [Deployment](/v0/operations/deployment)
- [agentctl CLI](/v0/reference/cli)
- [Permissions & grants](/v0/security/grants)
- [Observability](/v0/operations/observability)

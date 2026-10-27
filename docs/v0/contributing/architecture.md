# Architecture

This page is a map of how agent.d is built: the split between Rust and Lua, the crates in the workspace, and how a request moves from a connected client to a Lua handler and back. Read it before you change the code.

## The Rust / Lua boundary

agent.d is split into two layers with a hard boundary between them.

**Rust owns:**
- Policy enforcement and the permission engine
- Durable state (the `redb` memory store)
- Scheduling and the async runtime (Tokio)
- Transport and session management (HTTP, WebSocket)
- Process, filesystem, and network access
- Credential storage (OS keyring)
- Tracing and the JSONL trace sink
- The AI provider dispatch layer
- All external APIs

**Lua owns:**
- User-defined behavior: tool handlers, runner definitions, skill text, service bodies
- Composition of components via `agentd.*` registration calls
- Coordination logic (channels, timers, `parallel`, `async`/`await`)

Lua code reaches the host only through a `ctx` handle. Its methods are written in Rust, and each one passes the permission engine before it touches the host.

## Crate workspace

| Crate | Role |
|---|---|
| `daemon` | Binary entry point: loads config, wires crates, starts the HTTP/WebSocket server |
| `api` | WebSocket protocol types: envelope structs, public `/ws` and control `/control` message schemas |
| `executor` | Hot-swappable runtime core (wrapped in `ArcSwap`): owns loaded tools, runners, skills, and services; dispatches requests |
| `scripting` | Lua VM integration (mlua): exposes `agentd.*` registration API and all `ctx.*` capability methods to Lua |
| `luals` | LuaLS type-stub generator: writes `.luals/agentd.lua`, `.luals/project.lua`, `.luarc.json` |
| `permissions` | Default-deny permission engine: checks `[denial]` and the service and interface allowlists first, then asks about the runner allowlist, tool grants, and confirm, one at a time |
| `approvals` | Interactive approval plane: pushes `approval.request` events over `/control`, waits for `approvals.resolve`, handles timeout (deny on expiry) |
| `runners` | Runner dispatch: builds system prompt from skills, drives the model tool-use loop (capped at `max_turns` outside a session), returns `RunResult` |
| `skills` | Skill registry: loads `.md` skill files (YAML frontmatter + Markdown body) and provides lookup by name |
| `services` | Service supervisor: starts Lua service bodies as tasks, applies restart/backoff policy, exposes state and `last_error` |
| `shell` | `ctx.shell` implementation: argv-only process execution (no shell string), enforces `shell.exec[:<bin>]` grant |
| `fs` | `ctx.fs.*` implementation: path resolution with symlink/`..` canonicalisation before grant check |
| `net` | HTTP client, WebSocket client and SMTP mailer behind `ctx.http`, `ctx.ws` and `ctx.mailer`. It only moves bytes: `scripting` checks the `net:<host>` grant before calling it |
| `netbroker` | Windows only: a small service that changes Windows Filtering Platform rules for the daemon, so the daemon itself runs without admin rights |
| `process` | Builds host process commands, including Windows script shims |
| `sessions` | Durable chat sessions: each session's ordered turns, label, owner and token totals |
| `secrets` | `ctx.secret.*` implementation: OS keyring via the platform secret store, enforces `secret:<key>` grant |
| `memory` | `ctx.memory.*` implementation: durable namespaced key/value store (redb), enforces `memory.read/write` grants |
| `ai` | `ctx.ai.*` implementation: provider registry and dispatch; built-in providers: `anthropic`, `anthropic-cli`, `openai`, `codex`, `openai-cli` |
| `codex` | `codex app-server` JSON-RPC backend for the `codex` provider prefix |
| `mcp` | Loopback MCP server: exposes registered tools over HTTP JSON-RPC for `claude` CLI tool-use (`anthropic-cli` backend) |
| `packages` | Package manager: local fs + git operations, `index.toml` under `$XDG_DATA_HOME/agentd/packages` |
| `trace` | JSONL trace sink: writes structured event records to `trace_file`; `agentctl trace` streams from it |
| `types` | Shared domain types used across crates |
| `cli` | `agentctl` binary: parses subcommands, connects over WebSocket, implements `grants listen` approval loop |

## Hot-swappable executor

The executor is wrapped in an `ArcSwap<Executor>`, which lets `--watch` (hot reload) replace it while requests are running:

1. The file watcher detects a change to `init.lua`, an imported file, a skill `.md`, or `grants.toml`.
2. A new `Executor` is built from scratch by re-running the Lua entry file.
3. `ArcSwap::store` atomically replaces the pointer.
4. Requests already running hold an `Arc` clone of the old executor and finish on it without interruption.
5. New requests immediately pick up the new executor.
6. Durable memory (`redb`) and a connected approval operator survive across reloads, because they live outside the executor.

## Request dispatch

A client sends a message over `/ws`:

```
{ "id": 1, "method": "actions.call", "params": { "name": "git.status", "args": {} } }
```

The path through the runtime:

1. `daemon` accepts the WebSocket upgrade and authenticates the bearer token.
2. The connection loop reads the envelope and routes on `method`.
3. For `actions.call`, the dispatcher loads the current `Executor` via `ArcSwap`.
4. The `permissions` crate checks the denials, then the questions, for the caller's identity and the action's `requires` set. The executor asks each question and checks again after every answer.
5. If approved, `scripting` invokes the Lua handler with a `ctx` handle scoped to the approved permissions.
6. Each `ctx.*` call (shell, fs, http, …) re-checks its specific grant before touching the host.
7. The handler returns a Lua value; it is serialised to JSON and returned in `{ "id": 1, "ok": true, "result": … }`.
8. The `trace` crate writes a JSONL record for the call.

## Run checks

Before submitting a PR, run the full check suite:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The default test suite avoids network calls and live model requests. Optional integration tests are enabled with env vars:

| Variable | What it enables |
|---|---|
| `AGENTD_TEST_CLAUDE=1` | Live `claude` CLI provider tests |
| `AGENTD_TEST_CODEX=1` | Live `codex app-server` provider and MCP tests |
| `AGENTD_TEST_KEYRING=1` | Real OS-keyring secret-store tests |

## See also

- [Concepts: runtime](/v0/concepts/runtime): the runtime from a user's point of view
- [Concepts: permissions](/v0/concepts/permissions): how a call is decided
- [Reference: configuration](/v0/reference/configuration): daemon and runtime settings

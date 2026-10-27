# Runtime

The runtime is the daemon process at the center of agent.d. It loads your Lua components once at startup, checks permissions on every call, and makes your tools and runners available to connected clients.

## What the runtime does

When you start the daemon, it runs your entry file (`init.lua` by default). Everything registered while it runs (tools, actions, runners, skills, services) stays loaded for as long as the process runs.

After loading, the daemon opens three endpoints on `127.0.0.1:7777` (you can change the address):

| Endpoint | Purpose |
|---|---|
| `GET /health` | Liveness probe. Returns `ok` and needs no authentication. |
| `/ws` | Where clients call actions and runners. WebSocket, with a bearer token. |
| `/control` | Where approvers and administrators connect. WebSocket, with a separate admin token. |

The startup banner prints the Local, WS, and Control URLs and how many actions, runners, services, and skills were loaded. If `init.lua` raises an error while loading, the daemon prints it and stops.

## Lifecycle

```text
daemon starts
  → evaluates init.lua (imports, tool/action/runner/skill/service registrations)
  → applies grants.toml
  → starts services (each in its own supervised coroutine)
  → opens /health + /ws + /control
  → ready
```

Each call on `/ws` goes through the permission checks before any Lua code runs. If the call is approved, the action handler or runner receives a `ctx` handle limited to the capabilities that were approved. The result goes back to the client and is added to the trace log (`trace.jsonl`).

## Hot reload

In development you can pass `--watch` (or set `AGENTD_WATCH=1`) to turn on hot reload. The daemon watches `init.lua`, every `import()`-ed file, loaded skill `.md` sources, and `grants.toml`. When any of those change:

1. A new runtime is built and loaded in place.
2. Requests already running on the old runtime finish there.
3. The editor type stubs in `.luals/` are regenerated.

Durable memory (`ctx.memory`) and an approver connected on `/control` stay in place across reloads, because they belong to the daemon process.

::: tip Development workflow
See [Observability](/v0/operations/observability) for development and runtime inspection commands.
:::

## Observability

Every approved action call is added to the trace log as one JSON line. To follow it live, run:

```bash [release]
agentctl trace -f
```

```bash [cargo]
cargo run -p agentd-cli -- trace -f
```

The default trace path is `$XDG_STATE_HOME/agentd/trace.jsonl`. See [Observability](/v0/operations/observability) for the full event schema.

## See also

- [Observability](/v0/operations/observability)
- [Deployment](/v0/operations/deployment)
- [Configuration reference](/v0/reference/configuration)

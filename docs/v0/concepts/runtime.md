# Runtime

The runtime is the daemon process at the center of agent.d. It loads your Lua components once at startup, enforces the permission policy on every call, and exposes your tools and runners to connected clients.

## What the runtime does

When you start the daemon it evaluates your entry file (`init.lua` by default). Everything registered during that evaluation — tools, actions, runners, skills, services — is held in memory for the lifetime of the process.

After loading, the daemon opens three endpoints on `127.0.0.1:7777` (configurable):

| Endpoint | Purpose |
|---|---|
| `GET /health` | Open liveness probe. Always returns `ok`. No auth required. |
| `/ws` | Client data plane. WebSocket, bearer-token auth. |
| `/control` | Privileged operator and approval plane. WebSocket, separate admin token. |

The startup banner prints the Local, WS, and Control URLs alongside the counts of loaded actions, runners, services, and skills. If a component fails to register, the daemon logs the error and continues — a partially loaded runtime is better than no runtime.

## Lifecycle

```text
daemon starts
  → evaluates init.lua (imports, tool/action/runner/skill/service registrations)
  → applies grants.toml
  → starts services (each in its own supervised coroutine)
  → opens /health + /ws + /control
  → ready
```

Each inbound call on `/ws` flows through the permission engine before any Lua code runs. If the call is approved, the action handler or runner receives a `ctx` handle pre-scoped to only the capabilities the engine approved. The result is returned to the client and appended to the trace log (`trace.jsonl`).

## Hot reload

In development you can pass `--watch` (or set `AGENTD_WATCH=1`) to turn on hot reload. The daemon watches `init.lua`, every `import()`-ed file, loaded skill `.md` sources, and `grants.toml`. When any of those change:

1. A new runtime is built and loaded in place.
2. In-flight requests on the old runtime drain to completion.
3. The LuaLS type stubs in `.luals/` are regenerated.

Durable memory (`ctx.memory`) and a connected approval operator on `/control` survive reloads — they are owned by the daemon process, not the runtime.

::: tip Development workflow
See [Dev loop](/v0/tutorial/dev-loop) for a step-by-step guide to iterating with `--watch`.
:::

## Observability

Every approved action call is appended to the trace log as a JSONL event. Follow it live:

```bash [release]
agentctl trace -f
```

```bash [cargo]
cargo run -p agentd-cli -- trace -f
```

The default trace path is `$XDG_STATE_HOME/agentd/trace.jsonl`. See [Observability](/v0/operations/observability) for the full event schema.

## See also

- [Dev loop](/v0/tutorial/dev-loop)
- [Deployment](/v0/operations/deployment)
- [Observability](/v0/operations/observability)
- [Configuration reference](/v0/reference/configuration)

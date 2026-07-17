# Observability

How to monitor a running agent.d daemon: the JSONL trace sink, log-level filters, the startup banner, and what to watch for in production.

## Trace log

Every action call, runner invocation, permission decision, and service event is written as a JSONL record to the trace file.

**Default path:** `$XDG_STATE_HOME/agentd/trace.jsonl`

Override it with `--trace` or `AGENTD_TRACE` (the old `--trace-file` / `AGENTD_TRACE_FILE` names still work as deprecated aliases):

```bash
agentd --trace /var/log/agentd/trace.jsonl
```

Or in `config.toml`:

```toml
[daemon]
trace_file = "/var/log/agentd/trace.jsonl"
```

### Tailing the trace with agentctl

```bash
# Print the last 20 lines (default) and exit
agentctl trace

# Print the last 50 lines and exit
agentctl trace -n 50

# Follow live (like tail -f)
agentctl trace -f

# Read from a non-default file
agentctl trace --file /var/log/agentd/trace.jsonl -f -n 100
```

::: tip
`agentctl trace` reads the JSONL trace file directly from local disk — it does not connect to the daemon. The `--file` flag overrides the local path to read from (default: the state-dir `trace.jsonl`). Use it when the daemon was started with a custom `--trace` pointing to a different location.
:::

## Log level

The daemon emits structured tracing output to stderr. The log level is controlled by a filter string such as `warn`, `info`, or a per-target form like `warn,agentd=debug`.

**Precedence:** `--log` flag > `AGENTD_LOG` env var > `log_level` in `config.toml` > `RUST_LOG` env var > built-in default (`warn`).

| Example filter | Effect |
|---|---|
| `warn` | Only warnings and errors (default) |
| `info` | Startup events, each request handled |
| `debug` | Detailed request lifecycle |
| `trace` | Everything including internal spans |
| `agentd_executor=debug,warn` | Debug for the executor crate, warn for everything else |
| `agentd_executor=trace` | Full trace for the executor only |

Set it at launch:

```bash
agentd --log info
# or
export AGENTD_LOG=info
```

Or in `config.toml`:

```toml
[daemon]
log_level = "info"
```

## Startup banner

When the daemon starts successfully it prints a banner to stdout listing what was loaded and where to connect:

```
  AGENTD v0.3.0-alpha  ready in 34 ms

  Local:   http://127.0.0.1:7777/
  WS:      ws://127.0.0.1:7777/ws
  Control: ws://127.0.0.1:7777/control
  Loaded:  14 actions, 9 runners, 0 services, 9 skills
  Init:    /home/you/agents/init.lua
  Logs:    warnings/errors (AGENTD_LOG=debug for detail)
```

The `Loaded` line reports how many actions, runners, services, and skills the runtime registered. If the counts are lower than expected, check `init.lua` and any `import()` files for registration errors — these are logged at `warn` or `error` level (raise verbosity with `AGENTD_LOG=debug`).

## What to watch for

### Permission denials

A denied action call appears in the trace and in the log at `warn` level. If you see unexpected denials:

1. Check which grant is missing: `agentctl grants listen` shows the missing permissions in the approval request.
2. Add the grant to `grants.toml` under the appropriate `[tool.*]`, `[runner.*]`, `[service.*]`, or `[interface.*]` section.
3. Verify the `[policy]` section does not list the action in `deny_actions` — policy denials are hard and are never escalated to approval.

### Service health

Long-running services can crash and restart. Check their current state:

```bash
agentctl services ls
```

The output includes `state` (running, stopped, etc.) and `last_error` when a service has faulted. A service with `restart = "always"` or `restart = "on_failure"` will be restarted automatically after `backoff_ms` (default behaviour defined in the service registration).

::: warning
A service stuck in a restart loop will surface repeated errors in the trace log. Investigate `last_error` before assuming the daemon itself is unhealthy.
:::

### Liveness

The `/health` endpoint returns `ok` with HTTP 200 and requires no authentication. Use it as a liveness probe in your orchestrator or load balancer:

```bash
curl http://127.0.0.1:7777/health
```

## See also

- [Deployment](/v0/operations/deployment) — binding address, token management, systemd unit
- [Troubleshooting](/v0/operations/troubleshooting) — common issues and fixes
- [Reference: CLI](/v0/reference/cli) — full `agentctl trace` flag reference
- [Concepts: services](/v0/concepts/services) — service lifecycle and restart behaviour

# Observability

When an agent does something unexpected, or a service goes quiet, you need to see what the daemon did. This page covers the trace log, which records every call, the daemon's log level, and what to watch for in production.

## Trace log

The daemon writes every action call, runner run, permission decision and service event as one JSON line in the trace file.

**Default path:** `$XDG_STATE_HOME/agentd/trace.jsonl`

To change it, use `--trace` or `AGENTD_TRACE`. The old names `--trace-file` and `AGENTD_TRACE_FILE` still work, but they are deprecated:

```bash
agentd --trace /var/log/agentd/trace.jsonl
```

Or in `config.toml`:

```toml
[daemon]
trace_file = "/var/log/agentd/trace.jsonl"
```

### Tailing the trace with agentctl

`agentctl trace` prints the newest lines of the trace, or follows it live:

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
`agentctl trace` reads the trace file from local disk and does not connect to the daemon. By default it reads `trace.jsonl` in the state folder. If the daemon was started with a custom `--trace` path, pass the same path with `--file`.
:::

## Log level

The daemon also writes log messages to stderr. A [tracing](https://docs.rs/tracing-subscriber) filter string sets how much it writes.

The daemon uses the first of these that is set: the `--log` flag, the `AGENTD_LOG` environment variable, `log_level` in `config.toml`, the `RUST_LOG` environment variable, and finally the default, `warn`.

| Example filter | Effect |
|---|---|
| `warn` | Only warnings and errors (default) |
| `info` | Startup events, each request handled |
| `debug` | Detailed request lifecycle |
| `trace` | Everything including internal spans |
| `agentd_executor=debug,warn` | Debug for the part that runs actions and runners, warn for everything else |
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

## What to watch for

### Permission denials

A refused action call appears in the trace and in the log at `warn` level. If you see refusals you did not expect:

1. Check which grant is missing: `agentctl grants listen` shows the missing permissions in the approval request.
2. Add the grant to `grants.toml` under the appropriate `[tool.*]`, `[runner.*]`, `[service.*]`, or `[interface.*]` section.
3. Check that `[denial]` does not list the action or one of its permissions. Denied calls are refused without a question.

### Approval decisions

Each approval decision appears in the trace as an event with the kind `approval`. Its result tells you who decided (`human`, `classifier`, or `nobody`), the answer, and the classifier's reason when the classifier decided. The request in the event stays hidden, like all trace arguments. Use these events to see what a classifier allowed while you were away, or why a `nobody` run could not finish.

### Service health

Long-running services can crash and restart. To check their current state, run:

```bash
agentctl services ls
```

The output includes `state` (running, stopped, and so on) and, after a failure, `last_error`. A service with `restart = "always"` or `restart = "on_failure"` restarts on its own after a delay that starts at `backoff_ms` (1000 ms unless you set it when you register the service) and doubles after each failure in a row, up to `backoff_max_ms`.

::: warning
A service stuck in a restart loop shows repeated errors in the trace log. Read `last_error` before you suspect the daemon itself.
:::

### Liveness

The `/health` endpoint returns `ok` with HTTP 200 and requires no authentication. Use it as a liveness probe in your orchestrator or load balancer:

```bash
curl http://127.0.0.1:7777/health
```

## See also

- [Deployment](/v0/operations/deployment): binding address, token management, systemd unit
- [Troubleshooting](/v0/operations/troubleshooting): common issues and fixes
- [Reference: CLI](/v0/reference/cli): full `agentctl trace` flag reference
- [Concepts: services](/v0/concepts/services): service lifecycle and restart behaviour

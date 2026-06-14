# agentd-trace

Logs. Append-only execution trace.

- `TraceEvent` — one recorded dispatch / lifecycle event.
- `TraceSink` trait — pluggable sink.
- `JsonlSink` — JSONL append to a file (no sqlite yet).

The executor emits a `TraceEvent` for every dispatch, including service lifecycle.

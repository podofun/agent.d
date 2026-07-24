# agentd-trace

`agentd-trace` records append-only execution events.

`TraceEvent` stores the timestamp, action, duration, outcome, execution correlation ID, and event kind. Constructors redact every argument and result leaf before storage while preserving object keys and collection structure.

`TraceSink` defines the storage interface. `JsonlSink` appends one JSON object per line and flushes each event.

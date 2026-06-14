# agentd-api

Interface — the WebSocket surface. An axum router, two planes + a health probe.

- `GET /health` → `ok` (always open).
- `/ws` — **public data plane** (public bearer token). Methods: `tools.list`, `actions.call`, `runners.list|inspect|run`, `skills.list|inspect`, `services.list`.
- `/control` — **control plane** (distinct admin token). `approvals.subscribe` / `approvals.resolve`, plus server-pushed `approval.request` frames. A public-token holder can never reach it.

Envelope: `{ id, method, params? }` → `{ id, ok, result? | error?, code? }`. Each
connection gets a `ws-<n>` session id. **No HTTP action routes.**

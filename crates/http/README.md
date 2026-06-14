# agentd-http

HTTP client primitive. Built on `reqwest` + rustls.

`Request { method, url, headers, body, json, timeout_ms }` → `Response { status, headers, body }`.

`host_of(url)` helper derives the `net:<host>` slug. Permission gating lives in the
caller (scripting `ctx.http`).

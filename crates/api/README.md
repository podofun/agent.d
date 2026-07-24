# agentd-api

`agentd-api` provides the daemon's HTTP health probe and WebSocket API.

Routes:

- `GET /health` returns `ok` without authentication.
- `/ws` is the data plane for tools, actions, runners, skills, and services.
- `/control` is the operator plane for approval requests and decisions.

The data and control planes can use different bearer tokens. WebSocket requests use `{ "id", "method", "params" }`. Responses include the same `id`, an `ok` flag, and either a result or an error.

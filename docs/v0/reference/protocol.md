# WebSocket Protocol Reference

This page documents the JSON envelope format and every method available on the `/ws` and `/control` WebSocket endpoints. If you are calling agent.d from code rather than `agentctl`, this is your primary reference.

## Endpoints

| Endpoint | Transport | Auth | Purpose |
|---|---|---|---|
| `GET /health` | HTTP | none | Liveness probe — always open |
| `/ws` | WebSocket | bearer token | Client data plane |
| `/control` | WebSocket | admin bearer token | Operator / approval plane |

---

## Envelope format

Every `/ws` exchange is a request/response pair of JSON objects.

### Request (client → server)

```json
{
  "id": 1,
  "method": "actions.call",
  "params": { "name": "git.diff", "args": {} }
}
```

| Field | Type | Description |
|---|---|---|
| `id` | integer | Caller-chosen request id; echoed back in the response |
| `method` | string | Method name (see table below) |
| `params` | object \| null | Method-specific parameters; may be omitted for parameterless methods |

### Success response (server → client)

```json
{
  "id": 1,
  "ok": true,
  "result": { "result": "...", "duration_ms": 12 }
}
```

### Error response (server → client)

```json
{
  "id": 1,
  "ok": false,
  "code": "not_found",
  "error": "action `git.oops` not registered"
}
```

| Field | Type | Description |
|---|---|---|
| `id` | integer | Echoed request id |
| `ok` | boolean | `true` on success, `false` on error |
| `result` | any | Present when `ok: true` |
| `code` | string | Machine-readable error class; present when `ok: false` |
| `error` | string | Human-readable error message; present when `ok: false` |

---

## Authentication

### `/ws` token

Pass the bearer token on the WebSocket handshake:

```
Authorization: Bearer <token>
```

Token resolution order (same as `agentctl`):

1. `AGENTD_TOKEN` environment variable
2. `$XDG_STATE_HOME/agentd/token` — the file the daemon writes at startup
3. If neither exists, the daemon must be running with `--no-auth`

`/health` is always open and requires no token.

### `/control` token

The control plane uses a **separate** admin token so a public `/ws` token can never reach it.

1. `AGENTD_ADMIN_TOKEN` environment variable
2. `$XDG_STATE_HOME/agentd/admin-token`

---

## Caller identity

Every `/ws` connection receives a session id `ws-<n>` that is visible inside Lua handlers as `ctx.caller.session`. Bridging interfaces (Telegram, Discord, …) can override this per-request by passing `session` and `user` params on `actions.call` and `runners.run`. The resulting identity surface in Lua is:

```lua
ctx.caller.interface   -- always "ws" for WebSocket connections
ctx.caller.session     -- connection id (ws-<n>) or the overridden value
ctx.caller.user        -- caller-supplied user id, if provided
ctx.caller.runner      -- set when the request is a runners.run call
ctx.caller.execution   -- unique per top-level request (exec-<n>)
```

---

## `/ws` method reference

### `health`

Parameterless liveness check over the WebSocket.

**Params:** none

**Result:** `"ok"`

```json
{ "id": 1, "method": "health" }
// →
{ "id": 1, "ok": true, "result": "ok" }
```

---

### `tools.list`

List all registered action names.

**Params:** none

**Result:** `["tool.action", ...]`

```json
{ "id": 2, "method": "tools.list" }
// →
{ "id": 2, "ok": true, "result": ["git.diff", "git.status", "discord.send"] }
```

---

### `actions.call`

Invoke a registered action.

**Params:**

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Fully-qualified action name (`tool.action`) |
| `args` | object | no | Arguments passed to the handler |
| `session` | string | no | Override the per-connection session id |
| `user` | string | no | Caller-supplied user id |

**Result:** `{ "result": <any>, "duration_ms": <int> }`

```json
{ "id": 3, "method": "actions.call", "params": { "name": "git.status", "args": {} } }
// →
{ "id": 3, "ok": true, "result": { "result": "...", "duration_ms": 18 } }
```

---

### `runners.list`

List registered runners.

**Params:** none

**Result:** `[{ "name", "model", "skills", "allowed_actions" }, ...]`

```json
{ "id": 4, "method": "runners.list" }
// →
{
  "id": 4,
  "ok": true,
  "result": [
    { "name": "backend_reviewer", "model": "anthropic/claude-opus-4-7", "skills": ["reviewer"], "allowed_actions": ["git.diff", "git.status"] }
  ]
}
```

---

### `runners.inspect`

Return the full composition of a runner (resolved system prompt, skills, allowed actions).

**Params:** `{ "name": "<runner-name>" }`

**Result:** runner composition object

```json
{ "id": 5, "method": "runners.inspect", "params": { "name": "backend_reviewer" } }
```

---

### `runners.run`

Run a runner with a prompt and return its text output.

**Params:**

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Runner name |
| `prompt` | string | yes | User prompt |
| `session` | string | no | Override session id |
| `user` | string | no | Caller-supplied user id |

**Result:** `{ "text": "...", "provider": "...", "model": "...", "stop_reason"?: "..." }`

```json
{ "id": 6, "method": "runners.run", "params": { "name": "backend_reviewer", "prompt": "Review the diff" } }
// →
{ "id": 6, "ok": true, "result": { "text": "LGTM.", "provider": "anthropic", "model": "claude-opus-4-7", "stop_reason": "end_turn" } }
```

---

### `skills.list`

List registered skills.

**Params:** none

**Result:** `[{ "name", "description", "actions" }, ...]`

---

### `skills.inspect`

Return the full definition of a skill.

**Params:** `{ "name": "<skill-name>" }`

**Result:** skill definition object

---

### `services.list`

List background services with their current state.

**Params:** none

**Result:** `[{ "name", "state", "last_error"? }, ...]`

```json
{ "id": 9, "method": "services.list" }
// →
{ "id": 9, "ok": true, "result": [{ "name": "discord_handler", "state": "running" }] }
```

---

## Error codes

| Code | Meaning |
|---|---|
| `invalid_envelope` | Request JSON could not be parsed |
| `not_found` | Action, runner, or skill does not exist |
| `unknown_method` | The `method` field is not a known method |
| `bad_params` | Params failed to deserialize against the expected shape |
| `denied` | The permission engine denied the call |
| `needs_confirmation` | Action requires interactive approval; escalate via `/control` |
| `invocation_failed` | The action handler returned an error |
| `not_found` (runner) | Named runner does not exist |
| `unknown_skill` | Runner references a skill that is not registered |
| `no_provider` | No AI provider matched the runner's model prefix |
| `provider_upstream` | AI provider returned an error |

---

## `/control` plane

The control plane is a privileged WebSocket endpoint for operators. Connecting with a valid admin token automatically subscribes you as an approver — you do not need to send a subscribe message first, though `approvals.subscribe` is accepted as an idempotent ack.

### Subscribe-on-connect

Connecting to `/control` with a valid admin token immediately registers the connection as an approver. The server will push `approval.request` frames for every pending permission escalation.

### Push frame: `approval.request`

When an action triggers a `confirm = true` approval or the permission engine raises an escalation, the server pushes:

```json
{
  "event": "approval.request",
  "req": {
    "id": 7,
    "action": "shell.exec",
    "tool": "git",
    "kind": "confirm",
    "reason": "Running git push to origin",
    "missing": ["shell.exec:git"],
    "caller": {
      "runner": "backend_reviewer",
      "interface": null,
      "service": null,
      "session": "ws-3",
      "user": null
    }
  }
}
```

| Field | Description |
|---|---|
| `id` | Request id to echo in `approvals.resolve` |
| `action` | Fully-qualified action name |
| `tool` | Tool that owns the action |
| `kind` | Approval kind (`confirm`, etc.) |
| `reason` | Human-readable explanation from the action |
| `missing` | Permission slugs that are not yet granted |
| `caller` | Identity of the caller that triggered this request |

### `approvals.subscribe`

Idempotent ack. Returns `{ "subscribed": true }`.

```json
{ "id": 0, "method": "approvals.subscribe" }
```

### `approvals.resolve`

Send a verdict for a pending approval request.

```json
{
  "id": 1,
  "method": "approvals.resolve",
  "params": { "request_id": 7, "verdict": "allow_once" }
}
```

| Verdict | Effect |
|---|---|
| `allow_once` | Permit this single invocation |
| `allow_forever` | Permanently grant (stored in grants) |
| `deny` | Reject the invocation |

If no verdict arrives within `approval_timeout_ms` (default 120 000 ms) the request fails closed with a `deny`.

---

## See also

- [agentctl CLI](/v0/reference/cli)
- [Approvals](/v0/security/approvals)
- [Permissions & grants](/v0/security/grants)
- [Interfaces and callers](/v0/concepts/interfaces-and-callers)

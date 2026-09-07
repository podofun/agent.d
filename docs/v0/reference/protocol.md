# WebSocket Protocol Reference

This page documents the JSON envelope format and every method available on the `/ws` and `/control` WebSocket endpoints. If you are calling agent.d from code rather than `agentctl`, this is your primary reference.

## Endpoints

| Endpoint | Transport | Auth | Purpose |
|---|---|---|---|
| `GET /health` | HTTP | none | Liveness probe — always open |
| `GET /ready` | HTTP | none | Admission readiness; returns 503 while draining |
| `/ws` | WebSocket | bearer token | Client data plane |
| `/control` | WebSocket | admin bearer token | Operator / approval plane |

---

## Envelope format

Every `/ws` exchange is a request/response pair of JSON objects. Requests execute concurrently and responses may arrive out of order. Match responses and streaming events by `id`. Keep IDs unique until the final response arrives; reusing an active ID closes the connection because its responses would be ambiguous. A connection accepts at most 32 in-flight requests and rejects additional work with `busy`. WebSocket messages and frames are limited to 1,100,000 bytes.

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
  "error": "action `git.oops` not registered",
  "tip": "Run `agentctl tools` to list registered actions"
}
```

A failure raised from inside a Lua script additionally carries a cleaned traceback:

```json
{
  "id": 2,
  "ok": false,
  "code": "no_provider",
  "error": "runner `juma` could not resolve a provider for model `github/openai/gpt-4o-mini`",
  "tip": "You can configure new providers in your `config.toml`",
  "trace": ["helpers.lua:313  in structured", "init.lua:53"]
}
```

| Field | Type | Description |
|---|---|---|
| `id` | integer | Echoed request id |
| `ok` | boolean | `true` on success, `false` on error |
| `result` | any | Present when `ok: true` |
| `code` | string | Machine-readable error class; present when `ok: false` |
| `error` | string | Human-readable error message; present when `ok: false`. Always the innermost cause — no `invocation failed:` / `lua:` prefix chains |
| `tip` | string | Optional actionable hint for the error code; present only when the code has one |
| `trace` | array of strings | Optional cleaned Lua traceback frames (`file.lua:line  in fn`); present only for script failures |

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

Run a runner with a prompt, explicit message history, or both. The runner must hold `ai:<resolved-provider>` in `[runner.<name>].granted`, including when a per-call model selects another provider. Policy denials override this grant.

**Params:**

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Runner name |
| `prompt` | string | conditional | User prompt appended after history; required if history is empty |
| `messages` | array | conditional | Ordered history; `history` is an alias |
| `system` | string | no | Additional system instructions appended to the runner composition |
| `model` | string | no | Per-call model override; its provider must be granted to the runner |
| `max_tokens` | integer | no | Output token limit, 1–32768; provider support varies |
| `timeout_ms` | integer | no | Whole-run deadline, 1–600000 milliseconds; default 120000 |
| `session` | string | no | Override session id |
| `user` | string | no | Caller-supplied user id |
| `stream` | bool | no | Push `runner.delta` event frames while the run is in flight |

**Result:** `{ "text": "...", "provider": "...", "model": "...", "stop_reason"?: "..." }`

History messages require a valid `role` (`system`, `user`, `assistant`, or `tool`) and string `content`. Assistant messages may contain `tool_calls` (`id`, `name`, object `arguments`); tool results require the matching `tool_call_id`. Every tool call must receive exactly one result before another non-tool message. Unknown options, invalid histories, more than 256 history messages, or input text and tool-call data exceeding 1 MiB return `bad_params` before provider access.

`session` and `user` describe the caller; they do not store or restore conversation history. The client supplies history on each run. The runtime admits up to 32 concurrent runner invocations across connections and nested Lua calls; excess invocations return `busy` without queueing.

When every model turn reports token usage, the result also contains `usage` with cumulative `input_tokens`, `output_tokens`, `cache_read_input_tokens`, and `cache_creation_input_tokens`. Input tokens exclude the separately reported cache tokens. Missing usage is omitted rather than reported as zero; failed or cancelled runs do not return an accounting total. This is token reporting, not billing or quota enforcement.

Provider HTTP failures include `provider_status` and, when a numeric Retry-After header is present, `retry_after_ms`. These describe the provider response, not permission to replay the whole run: earlier tool calls may already have changed external state. agent.d does not automatically retry runs.


```json
{ "id": 6, "method": "runners.run", "params": { "name": "backend_reviewer", "prompt": "Review the diff" } }
// →
{ "id": 6, "ok": true, "result": { "text": "LGTM.", "provider": "anthropic", "model": "claude-opus-4-7", "stop_reason": "end_turn" } }
```

**Streaming.** With `stream: true` the server pushes event frames on the same connection while the run is in flight, then sends the ordinary complete response envelope as the final frame. On success the final response contains the complete result. Deltas are provisional: provider errors, incomplete streams, cancellation, or deadlines can still end the run with an error. Publish or persist the completed answer only after the final success envelope.

```json
{ "event": "runner.delta", "id": 6, "delta": { "type": "text_delta", "text": "LG" } }
{ "event": "runner.delta", "id": 6, "delta": { "type": "text_delta", "text": "TM." } }
{ "event": "runner.delta", "id": 6, "delta": { "type": "tool_call", "name": "git.diff" } }
{ "event": "runner.delta", "id": 6, "delta": { "type": "turn_end" } }
{ "id": 6, "ok": true, "result": { "text": "LGTM.", "provider": "anthropic", "model": "claude-opus-4-7", "stop_reason": "end_turn" } }
```

Delta types: `text_delta` (a chunk of assistant text, in order), `tool_call` (the model asked for a tool), `turn_end` (one provider turn finished). Event frames carry the request `id`, so a client multiplexing requests can route them. Providers without incremental output send no deltas; the final envelope arrives either way.

Streaming buffers are bounded. If a consumer cannot keep up, the run fails with `slow_consumer`; socket writes have a five-second deadline. Disconnecting drops in-flight runner futures and stops further model turns. Cancellation and disconnect do not undo tools already dispatched or external side effects.

### `runners.cancel`

Request cancellation of an in-flight runner on the same connection:

```json
{ "id": 7, "method": "runners.cancel", "params": { "id": 6 } }
```

The cancellation request returns `{ "cancelled": true }` when delivered, or `false` if the target is absent, finished, or already being cancelled. The target request receives its own final response, normally with `code: "cancelled"`; completion can win a race with cancellation. Wait for that final response before reusing its ID. A whole-run deadline returns `timeout`.

These are connection-local requests, not durable jobs. IDs do not deduplicate work after reconnecting. Integrations that retry side-effecting work need their own durable event IDs, result storage, and idempotent actions. Signed webhooks likewise authenticate payloads without providing deduplication or replay protection.

On SIGINT or SIGTERM the daemon stops admitting work and allows up to 30 seconds for active requests to drain. `/ready` returns 503 while `/health` remains a liveness check. Readiness does not make a paid provider request or verify remote credentials.

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

| Code | Meaning | Tip |
|---|---|---|
| `invalid_envelope` | Request JSON could not be parsed | — |
| `not_found` | Action or skill does not exist | Run `agentctl tools` to list registered actions |
| `unknown_method` | The `method` field is not a known method | — |
| `bad_params` | Params failed to deserialize against the expected shape | Pass args with `-d key=value` or `--json '<json>'` |
| `denied` | The permission engine denied the call | Adjust `grants.toml`, or approve live via `agentctl grants listen` |
| `needs_confirmation` | Action requires interactive approval; escalate via `/control` | Adjust `grants.toml`, or approve live via `agentctl grants listen` |
| `lua_error` | A Lua script raised an error; `trace` carries the cleaned traceback | — |
| `invocation_failed` | The action failed outside the script itself (e.g. join errors, output validation) | — |
| `runner_not_found` | Named runner does not exist | Run `agentctl runner ls` to list runners |
| `unknown_skill` | Runner references a skill that is not registered | Run `agentctl skills ls` to list skills |
| `no_provider` | The runner could not resolve a provider for its model | You can configure new providers in your `config.toml` |
| `provider_upstream` | AI provider returned an error | — |

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

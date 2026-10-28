# WebSocket Protocol Reference

This page documents the JSON envelope format and every method available on the `/ws` and `/control` WebSocket endpoints. Use it when you call agent.d from your own code. For the command line, see [agentctl](/v0/reference/cli).

## Endpoints

| Endpoint | Transport | Auth | Purpose |
|---|---|---|---|
| `GET /health` | HTTP | none | Liveness probe, open without a token |
| `GET /ready` | HTTP | none | Admission readiness; returns 503 while draining |
| `/ws` | WebSocket | bearer token | Client data plane |
| `/control` | WebSocket | admin bearer token | Operator / approval plane |

---

## Envelope format

Every `/ws` exchange is a request/response pair of JSON objects. Requests execute concurrently and responses may arrive out of order. Match responses and streaming events by `id`. Keep each ID unique until its final response arrives. Reusing an ID that is still active closes the connection, because the responses could not be told apart. A connection accepts at most 32 in-flight requests and rejects additional work with `busy`. WebSocket messages and frames are limited to 1,100,000 bytes.

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

A failure raised inside a Lua script also carries a cleaned traceback:

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
| `error` | string | Human-readable error message; present when `ok: false`. It states the innermost cause, without `invocation failed:` or `lua:` prefixes |
| `tip` | string | Optional actionable hint for the error code; present only when the code has one |
| `trace` | array of strings | Optional cleaned Lua traceback frames (`file.lua:line  in fn`); present only for script failures |

---

## Authentication

### `/ws` token

Pass the bearer token on the WebSocket handshake:

```
Authorization: Bearer <token>
```

`agentctl` picks its `/ws` token in this order:

1. The `AGENTD_TOKEN` environment variable. Use it to connect as an ordinary client.
2. The admin token: `AGENTD_ADMIN_TOKEN`, or `$XDG_STATE_HOME/agentd/admin-token`. See [Admin token on `/ws`](#admin-token-on-ws).
3. `$XDG_STATE_HOME/agentd/token`, the public token file that the daemon writes at startup.
4. If none exists, the daemon must be running with `--no-auth`.

`/health` requires no token.

### Admin token on `/ws`

`/ws` also accepts the admin token (see [`/control` token](#control-token)). A connection that presents it is an **admin connection** for as long as it stays open. It works like the `ws` interface, with two differences:

- It can make a run's approval setting looser. See [Where the setting comes from](/v0/security/approvals#where-the-setting-comes-from).
- Its sessions belong to admin connections only (`owner` is `admin:ws`). Other connections cannot see or use them.

With `--no-auth` there are no tokens, so every `ws` connection is an admin connection. A connection with an interface token is an ordinary connection of that interface.

### Interface tokens

Besides the daemon token, `config.toml` can declare `[interfaces.<name>]` entries, each with its own bearer token. A connection presenting one is interface `<name>` for every request it makes: `ctx.caller.interface` reports it, `[interface.<name>]` grants apply, and chat sessions it creates belong to it. The daemon token remains interface `ws`. See [configuration](/v0/reference/configuration#interfaces-name-sections).

### `/control` token

The control plane accepts only the admin token, which is separate from the public `/ws` token. `agentctl` reads it from:

1. `AGENTD_ADMIN_TOKEN` environment variable
2. `$XDG_STATE_HOME/agentd/admin-token`

---

## Caller identity

Each `/ws` connection gets a session id of the form `ws-<n>`, which Lua handlers see as `ctx.caller.session`. A bridge, such as a Telegram or Discord interface, can set its own `session` and `user` on each `actions.call` and `runners.run` request. Handlers see the identity as:

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

Liveness check over the WebSocket. It takes no parameters.

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

Run a runner with a prompt, an explicit message history, or a stored session. The runner must hold `ai:<resolved-provider>` in `[runner.<name>].granted`, including when a per-call model selects another provider. Policy denials override this grant.

**Params:**

| Field | Type | Required | Description |
|---|---|---|---|
| `name` | string | yes | Runner name |
| `prompt` | string | conditional | User prompt appended after history; required if history is empty |
| `messages` | array | conditional | Ordered history; `history` is an alias. Not allowed together with `session_id` |
| `session_id` | string | no | A session from `sessions.create`. Its turns become the history and the exchange is stored afterwards. Requires `prompt`; the session must be visible to this caller's interface and `user` |
| `system` | string | no | Additional system instructions appended to the runner composition |
| `model` | string | no | Per-call model override; its provider must be granted to the runner |
| `max_tokens` | integer | no | Output token limit, from 1 to 32768; provider support varies |
| `timeout_ms` | integer | no | Whole-run deadline, from 1 to 600000 milliseconds. Defaults to 120000 for a run without `session_id`. A session run has no deadline unless you set one, and keeps going until it finishes or is cancelled with `runners.cancel` |
| `session` | string | no | Override session id |
| `user` | string | no | Caller-supplied user id |
| `stream` | bool | no | Push `runner.delta` event frames while the run is in flight |
| `approval` | string | no | Who answers this run's questions: `human`, `classifier`, or `nobody`. A value looser than the runner's setting is `approval_forbidden`, unless the connection is an [admin connection](#admin-token-on-ws) |

**Result:** `{ "text": "...", "provider": "...", "model": "...", "stop_reason"?: "...", "session_id"?: "..." }`

History messages require a valid `role` (`system`, `user`, `assistant`, or `tool`) and string `content`. Assistant messages may contain `tool_calls` (`id`, `name`, object `arguments`); tool results require the matching `tool_call_id`. Every tool call must receive exactly one result before another non-tool message. Unknown options, invalid histories, more than 256 history messages, or input text and tool-call data exceeding 1 MiB return `bad_params` before provider access.

`session` and `user` describe the caller for permission checks. To have the daemon keep conversation history, create a session with `sessions.create` and pass its id as `session_id`.

A session runs one run at a time, and a second run on a busy session returns `session_busy`. When a run fails or is cancelled, the session keeps the prompt and every step whose tool calls all returned, followed by an assistant message saying the reply stopped before it finished. A compaction that finished before the failure stays stored.

Saved tool messages (role `tool`) carry the files the call changed, in `changes`, and how many more were left out, in `more`, in the same shape as on `tool_result` deltas below. Neither field is sent to a model.

The daemon runs up to 32 runners at once, counting all connections and nested Lua calls. A run over that limit returns `busy` immediately, without waiting in a queue.

When every model turn reports token usage, the result also contains `usage` with cumulative `input_tokens`, `output_tokens`, `cache_read_input_tokens`, and `cache_creation_input_tokens`. Input tokens exclude the separately reported cache tokens. When a turn does not report usage, the `usage` field is left out. Failed and cancelled runs return no total. These numbers report tokens and do not enforce billing or quotas.

Provider HTTP failures include `provider_status` and, when the provider sent a numeric Retry-After header, `retry_after_ms`. These describe the provider's response. Before you replay a whole run, keep in mind that earlier tool calls may already have changed external state. agent.d does not retry runs automatically.


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
{ "event": "runner.delta", "id": 6, "delta": { "type": "tool_result", "name": "git.diff", "input": {}, "output": "+one line" } }
{ "event": "runner.delta", "id": 6, "delta": { "type": "turn_end" } }
{ "id": 6, "ok": true, "result": { "text": "LGTM.", "provider": "anthropic", "model": "claude-opus-4-7", "stop_reason": "end_turn" } }
```

Delta types are `text_delta` (a chunk of assistant text, in order), `tool_call` (the model asked for a tool), `tool_result` (the tool finished) and `turn_end` (one provider turn finished).

A `tool_result` carries the tool's `name`, the `input` the model passed, and its `output`, or an `error` when it failed. Secrets the run read appear as `<secret name>` in all of them. When the call changed files, by `ctx.fs` or by any program it ran, `changes` lists them, newest last and at most 200, with `more` counting the rest:

| Field | Type | Description |
|---|---|---|
| `path` | string | The file, relative to the call's folder |
| `operation` | string | `created`, `modified`, or `removed` |
| `revision` | integer or null | The file's history revision this change made, when it can be undone |
| `diff` | string or null | A unified diff, with secrets replaced. Absent for binary files and files too large to show |
| `binary` | boolean | The file is not text |
| `added` | integer | Lines added |
| `removed` | integer | Lines removed | Event frames carry the request `id`, so a client with several requests in flight can route them. Providers without incremental output send no deltas, and the final envelope still arrives.

Streaming buffers are bounded. If a consumer cannot keep up, the run fails with `slow_consumer`; socket writes have a five-second deadline. Disconnecting drops in-flight runner futures and stops further model turns. Cancellation and disconnect do not undo tools already dispatched or external side effects.

### `runners.cancel`

Request cancellation of an in-flight runner on the same connection:

```json
{ "id": 7, "method": "runners.cancel", "params": { "id": 6 } }
```

The cancellation request returns `{ "cancelled": true }` when delivered, or `false` if the target is absent, finished, or already being cancelled. The target request receives its own final response, normally with `code: "cancelled"`; completion can win a race with cancellation. Wait for that final response before reusing its ID. A whole-run deadline returns `timeout`.

Requests live only as long as their connection. After a reconnect, a request ID does not prevent the same work from running twice. Integrations that retry work with side effects need their own durable event IDs, result storage and idempotent actions. Signed webhooks authenticate payloads and also leave deduplication and replay protection to you.

On SIGINT or SIGTERM the daemon stops admitting work and allows up to 30 seconds for active requests to drain. `/ready` returns 503 while `/health` remains a liveness check. Readiness does not make a paid provider request or verify remote credentials.

---

### `sessions.create`

Creates a chat session, which stores conversation history in the daemon. The result contains the session metadata. Pass its `id` as `session_id` in `runners.run`.

The optional parameters are `label`, `runner`, `approval`, `cwd`, `session` and `user`, all strings. A label is a name you choose, such as a chat id or ticket number. Labels are unique per interface across all users, and a label that is already in use returns `session_label_taken`. The `runner` field is for your own reference; any runner can use the session. The `approval` field sets who answers questions for runs in the session (`human`, `classifier`, or `nobody`); see [`sessions.set_approval`](#sessions-set-approval). The `session` and `user` fields set the caller identity, as they do for `actions.call` and `runners.run`.

The `cwd` field is the folder the session's runs work in, as an absolute path. Only connections that use the daemon token or the admin token can set it; a connection with an interface token cannot.

agent.d resolves the folder to its real path. It accepts the folder when it, or a folder above it, is listed under [`[folders]`](/v0/security/grants#folders) in grants.toml. For any other folder, it asks a person through the [approval channel](#control-plane) with the kind `folder`, and the request waits for the answer.

A refused folder returns `session_folder_denied`. A folder this connection may not choose, or one that is missing, relative, not a folder or the root of the disk, returns `session_folder_not_allowed`. A taken label is reported before anyone is asked about the folder.

The session belongs to the connection's interface (`interface:ws` for the daemon token, `interface:<name>` for an [interface token](#interface-tokens), or `admin:ws` for an [admin connection](#admin-token-on-ws)). If you pass `user`, later requests reach the session only when they carry the same user. Without `user`, the session is visible to all callers from that interface.

Send this request over `/ws`:

```json
{ "id": 10, "method": "sessions.create", "params": { "label": "telegram-42", "user": "alice" } }
```

A successful response has `ok: true` and the session metadata in `result`, including its generated `id`, `owner`, `label`, `user`, `turn_count`, `next_seq`, `compactions`, `usage`, `created_at` and `updated_at`. Timestamps are Unix seconds. The `usage` object adds up the tokens every run in the session has used, in `input_tokens`, `output_tokens`, `cache_read_input_tokens` and `cache_creation_input_tokens`. Sessions created before agent.d tracked usage count from zero.

### `sessions.get`

Gets a session and its stored messages by `id` or `label`. Pass the same `user` used when creating the session. A missing session or one outside the caller's scope returns `session_not_found`.

```json
{ "id": 11, "method": "sessions.get", "params": { "label": "telegram-42", "user": "alice" } }
```

The result contains the session metadata and a `turns` array, ordered from oldest to newest. For the newly created session above, `turns` is empty.

### `sessions.list`

Lists visible sessions from newest to oldest. The optional parameters are `limit` (an integer, default 50, capped at 500), `session` and `user` (strings). Without `user`, the list contains only sessions that have no user attached.

```json
{ "id": 12, "method": "sessions.list", "params": { "limit": 50, "user": "alice" } }
```

### `sessions.rename`

Changes a session's label. Pass the required string `id`, the new `label` (omit it or send `null` to clear the label), and if needed the string parameters `session` and `user`. Labels stay unique per owner, so a label another session already uses returns `session_label_taken`. A session outside the caller's scope returns `session_not_found`. The result is the updated session metadata.

```json
{ "id": 13, "method": "sessions.rename", "params": { "id": "8f3a1c2d-…", "label": "bugfix", "user": "alice" } }
```

### `sessions.set_approval`

Changes who answers questions for runs in a session. Pass the required string `id`, and `approval` as `human`, `classifier`, or `nobody`. Send `null` to clear the setting. You can also pass the string parameters `session` and `user`. The result is the updated session metadata.

Any caller that can see a session can change its setting. agent.d checks the value each time a run starts. A stricter value takes effect for every run, and a looser value takes effect only for runs that an admin connection starts. A change applies from the next run; a run already in progress keeps its setting. `classifier` returns `classifier_not_configured` when `[approval].classifier` in `grants.toml` names no runner.

```json
{ "id": 14, "method": "sessions.set_approval", "params": { "id": "8f3a1c2d-…", "approval": "nobody" } }
```

### `sessions.delete`

Deletes a session and its stored messages. Pass the required string `id` and, if needed, the string parameters `session` and `user`. The result is `{ "deleted": true }` if the session was deleted, or `{ "deleted": false }` if no visible session matched.

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
| `invalid_envelope` | Request JSON could not be parsed | |
| `not_found` | Action or skill does not exist | Run `agentctl tools` to list registered actions |
| `unknown_method` | The `method` field is not a known method | |
| `bad_params` | Params failed to deserialize against the expected shape | Pass args with `-d key=value` or `--json '<json>'` |
| `denied` | The permission engine denied the call | Adjust `grants.toml`, or approve live via `agentctl grants listen` |
| `needs_confirmation` | Action requires interactive approval; escalate via `/control` | Adjust `grants.toml`, or approve live via `agentctl grants listen` |
| `lua_error` | A Lua script raised an error; `trace` carries the cleaned traceback | |
| `invocation_failed` | The action failed outside the script itself, for example in output validation | |
| `runner_not_found` | Named runner does not exist | Run `agentctl runner ls` to list runners |
| `session_not_found` | The session does not exist or is not visible to this caller | Run `agentctl session ls` to list sessions, or `agentctl session new` to start one |
| `session_busy` | Another run is using that session | Wait for it to finish, then retry |
| `session_label_taken` | `sessions.create` reused a label | Fetch the existing one with `sessions.get { label }` |
| `session_folder_not_allowed` | `sessions.create` asked for a folder this connection may not choose, or one that cannot be used | Start the chat on this machine, in an existing project folder |
| `session_folder_denied` | Nobody trusted the folder `sessions.create` asked for | Start the chat from a folder you trust, or add it under `[folders] trusted` in grants.toml |
| `sessions_unavailable` | No session store is configured | |
| `unknown_skill` | Runner references a skill that is not registered | Run `agentctl skills ls` to list skills |
| `no_provider` | The runner could not resolve a provider for its model | You can configure new providers in your `config.toml` |
| `runner_turn_limit` | The runner used all `runtime.max_turns` turns without finishing. Only runs outside a session have this limit | Run it in a session to let it continue without a limit, or raise `max_turns` under `[runtime]` in `config.toml` |
| `provider_upstream` | AI provider returned an error | |
| `approval_forbidden` | A run asked for an approval setting looser than its runner or session allows, from a connection that is not an admin connection | Connect with the admin token, or use a stricter setting |
| `classifier_not_configured` | A session was set to `classifier`, but `[approval].classifier` names no runner | Name a classifier runner in `[approval].classifier` in grants.toml, or use `human` or `nobody` |

---

## `/control` plane

The control plane is where a person answers permission requests. It is a privileged WebSocket endpoint for operators, and `agentctl grants listen` and `agentctl chat` use it.

### Subscribe-on-connect

A connection to `/control` with a valid admin token becomes an approver as soon as it opens. The server then pushes an `approval.request` frame for each request that needs a person. You can send `approvals.subscribe`, but it is not required.

### Push frame: `approval.request`

When a call needs an approval and the run's setting sends it to a human, the server pushes:

```json
{
  "event": "approval.request",
  "req": {
    "id": 7,
    "kind": "missing_grant",
    "action": "dev.shell",
    "tool": "dev",
    "reason": "the handler asked for `shell.exec:cargo` mid-run, which tool `dev` has not been granted",
    "requires": ["shell.exec:cargo"],
    "missing": ["shell.exec:cargo"],
    "detail": { "argv": ["cargo", "test"], "cwd": "/home/me/project" },
    "run_id": "run-12",
    "session_id": "8f3a1c2d-…",
    "caller": {
      "runner": "coder",
      "interface": "ws",
      "session": "ws-3"
    }
  }
}
```

| Field | Description |
|---|---|
| `id` | Request id to echo in `approvals.resolve` |
| `kind` | Why agent.d asks: `missing_grant`, `runner_action`, `confirm`, or `folder` |
| `action` | Fully-qualified action name |
| `tool` | Tool that owns the action |
| `reason` | Human-readable explanation |
| `requires` | Every permission the action or call needs |
| `missing` | Permission slugs that are not yet granted |
| `detail` | What the call does: an action's arguments, or a command's `argv` and `cwd`. Secret values are replaced by `<secret name>`. Present when known |
| `run_id` | The run that asks. Present when the call belongs to a run |
| `session_id` | The chat session of the run. Present when the run uses one |
| `warning` | Text to show before "allow forever", for example for an interpreter. Present when needed |
| `caller` | Identity of the caller that triggered this request |

Questions that the classifier answers, or that `nobody` refuses, are not pushed. See [Interactive approvals](/v0/security/approvals#who-answers).

### `approvals.subscribe`

Confirms the subscription. Calling it again has no further effect. Returns `{ "subscribed": true }`.

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
| `allow_forever` | Grant the permission permanently (saved in grants) |
| `deny` | Reject the invocation |

If no verdict arrives within `approval_timeout_ms` (default 120 000 ms), the request is denied.

---

## See also

- [agentctl CLI](/v0/reference/cli)
- [Approvals](/v0/security/approvals)
- [Permissions & grants](/v0/security/grants)
- [Interfaces and callers](/v0/concepts/interfaces-and-callers)

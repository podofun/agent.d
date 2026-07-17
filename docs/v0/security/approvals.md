# Interactive approvals

When a call lacks a required grant or hits an action marked `confirm = true`, agent.d escalates to an interactive approval request over the `/control` endpoint. This page explains when escalation happens, how to handle it, and what the possible outcomes are.

## When escalation happens

An approval request is raised when:

1. **A required permission is missing** from `grants.toml` for the calling context (tool, runner, interface, or service layer).
2. **An action is marked `confirm = true`** in its registration — every invocation requires explicit operator approval, regardless of grants.

::: warning
Policy denials (`[policy].deny_actions` and `[policy].deny_permissions`) and allowlist denials are **hard denials** — they are never escalated to approval. The call fails immediately. Only missing grants and `confirm = true` actions reach the approval queue.
:::

## The approval loop with `agentctl grants listen`

Run `agentctl grants listen` to connect to `/control` and handle incoming requests interactively:

```bash
agentctl grants listen
```

The command subscribes as an approver and blocks, printing each request as it arrives. For each request you choose one of three verdicts:

| Verdict | Effect |
|---|---|
| `allow_once` | Permits this single call; the next identical request escalates again. |
| `allow_forever` | Permits this call and appends the action to `[policy].auto_confirm` in `grants.toml`. Future calls are pre-approved without escalation. |
| `deny` | Rejects the call; the caller receives an error. |

## Request fields

Each approval request contains:

| Field | Description |
|---|---|
| `id` | Unique request identifier used when resolving. |
| `action` | Fully-qualified action name (e.g. `git.status`). |
| `tool` | The tool that owns the action. |
| `kind` | Why the request was raised (`missing_grant` or `confirm`). |
| `reason` | Human-readable explanation of what permission is needed. |
| `missing[]` | The specific permission slugs that are absent. |
| `caller.runner` | Runner name, if the call came from a runner. |
| `caller.interface` | Interface name, if the call came from an interface. |
| `caller.service` | Service name, if the call came from a service. |
| `caller.session` | WebSocket session identifier. |
| `caller.user` | User identity passed by the client, if any. |

## Timeout behavior

If no approver responds within `approval_timeout_ms` (default 120 000 ms / 2 minutes), the request **fails closed** — the call is denied. The timeout is configurable via `--approval-timeout` or `AGENTD_APPROVAL_TIMEOUT_MS`.

::: warning
There is no retry or queuing after a timeout. The calling action receives an error, and the client must retry the operation if appropriate.
:::

## `allow_forever` and `grants.toml`

Choosing `allow_forever` for a `confirm = true` action appends the action name to `[policy].auto_confirm` in `grants.toml`. You can also add entries there directly:

```toml
[policy]
auto_confirm = ["git.status", "git.diff"]
```

Pre-approved actions bypass the approval queue entirely on subsequent calls.

## Resolving approvals programmatically

The `/control` WebSocket protocol exposes two methods for building custom approval tooling:

- `approvals.subscribe` — idempotent; re-registers you as an approver on an existing connection.
- `approvals.resolve { request_id, verdict }` — submit a verdict (`allow_once`, `allow_forever`, or `deny`).

The server pushes requests as events:

```json
{
  "event": "approval.request",
  "req": {
    "id": 42,
    "action": "git.push",
    "tool": "git",
    "kind": "confirm",
    "reason": "action requires interactive approval",
    "missing": [],
    "caller": {
      "runner": "backend_reviewer",
      "session": "ws-3"
    }
  }
}
```

See [WebSocket protocol](/v0/reference/protocol) for the full envelope format and [CLI reference](/v0/reference/cli) for `agentctl grants listen` flags.

## See also

- [grants.toml reference](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)
- [WebSocket protocol](/v0/reference/protocol)
- [CLI reference](/v0/reference/cli)

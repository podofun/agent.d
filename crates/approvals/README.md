# agentd-approvals

Interactive permission approvals. Transport-agnostic.

`Broker` holds a connected-approver registry + a pending-request registry, fans each
request out to all approvers, and awaits the first verdict with a timeout.

- `request()` (the `ApprovalBroker` impl) — returns `Deny` if there's no approver or on timeout (**fail closed**).
- `subscribe()` / `resolve()` — the control-transport side.

Knows nothing about WebSocket; `agentd-api`'s `/control` plane drives it.

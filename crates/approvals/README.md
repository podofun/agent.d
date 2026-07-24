# agentd-approvals

`agentd-approvals` coordinates permission decisions between the executor and operator clients.

The `Broker`:

- Assigns an ID to each approval request.
- Sends requests to all subscribed approvers.
- Resolves a request when an approver returns a verdict.
- Denies a request when no approver is connected or the request times out.

Transport code is outside this crate. `agentd-api` exposes the broker on the control WebSocket.

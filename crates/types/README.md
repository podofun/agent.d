# agentd-types

`agentd-types` contains transport-neutral types and traits shared across runtime crates.

It defines:

- Action calls, results, registry metadata, and registry errors.
- Call context and the `Registry` dispatch interface.
- Executor bridges for actions and runners.
- Approval requests, verdicts, and the approval-broker interface.
- Service options shared with Lua registration.

The crate contains boundary contracts. Implementations remain in the executor, scripting, approvals, and services crates.

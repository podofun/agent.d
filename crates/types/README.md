# agentd-types

Shared vocabulary for the workspace. No logic, no I/O.

Defines the core DTOs and traits every other crate speaks:

- `ActionCall`, `ActionResult` — one dispatch in/out.
- `Registry` trait + `RegistryError` — action lookup.
- `Dispatcher` — invoke an action (and `check_grants` for permission-only checks).
- Approval DTOs/trait — `ApprovalRequest`, `ApprovalKind`, `Verdict`, `ApprovalBroker`.

`types` is the leaf of the dependency graph; nothing here depends on the rest of agentd.

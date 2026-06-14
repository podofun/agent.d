# agentd-executor

Execution / scheduler. The universal kernel — no Lua dep.

`Executor` holds every registry (actions via `dyn Registry`, runners, services,
skills, providers). Methods:

- `run_action` — dispatch one action through the 5-layer permission engine.
- `run_runner` — drives the tool-use loop. Owns it for `ExecutorOwned` providers (composes the tool catalog, dispatches each tool call back through `run`, re-prompts to a 16-turn cap); binds the MCP loopback for `ProviderOwned` providers.
- `start_service` / `start_services` — supervises service lifecycle with restart policy.

Emits a `TraceEvent` for every dispatch. When wired to an `ApprovalBroker`, routes
escalatable denials to interactive approval (`AllowOnce` overlay, `AllowForever`
appends to `grants.toml` + hot-swaps a reloaded engine).

# agentd-services

`agentd-services` stores definitions and lifecycle state for long-running handlers.

A `ServiceDef` contains the service name, restart policy, and backoff limits. The `ServiceRegistry` stores definitions and reports pending, running, stopped, or crashed state.

The handler implementation remains in the action registry, such as `LuaHost`. The executor supervises handlers and applies the configured restart policy and backoff.

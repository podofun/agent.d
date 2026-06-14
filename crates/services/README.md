# agentd-services

Service — a long-running named Lua task (Discord gateway loop, IMAP idle, cron poll).

Storage only — execution lives in `agentd-executor::start_service`.

- `ServiceDef { name, tool, source }`.
- `ServiceRegistry`.
- `ServiceState { Pending, Running, Stopped, Crashed }` + `ServiceStatus`.

Registered in Lua via `agentd.service(name, fn)`.

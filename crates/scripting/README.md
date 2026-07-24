# agentd-scripting

`agentd-scripting` hosts the sandboxed Lua 5.4 userland.

`LuaHost` loads scripts and registers tools, actions, runners, skills, and services. Action contexts expose approved filesystem, shell, HTTP, WebSocket, mail, memory, secret, logging, nested-call, runner, and concurrency operations.

The crate also provides coroutine scheduling, channels, imports, Lua error cleanup, and runtime API tables. Rust supplies host capabilities and effective grants; Lua defines user workflows.

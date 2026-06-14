# agentd-scripting

Lua host. `LuaHost` is a `Registry` impl backed by mlua, owning the Lua state +
catalog (actions, services).

Provides the sandboxed userland:

- Registration: `import`, `agentd.tool`, `agentd.action`, `agentd.runner`, `agentd.skill`, `agentd.skills.load/dir`, `agentd.service`.
- The per-invocation `ctx` capability handle (fs, http, ws, shell, secret, memory, ai, call, run, caller, log, state) — injected as the handler/service parameter, never a global. Every binding does an inline permission check.
- Bare globals: `async`, `await`, `channel`, `sleep`, `timer`, `json`, `pcall`.
- Cooperative scheduler — yieldable IO across Lua coroutines.

Stdlib lockdown lives in the `sandbox` module (`lock_down(&Lua)`): strips
`io`/`os`/`package`/`debug`/`require`/`load*`/metatable escapes; keeps
`string`/`table`/`math`/`coroutine`/`utf8` + safe basics + `agentd`.

Full surface: `docs/lua-reference.md`.

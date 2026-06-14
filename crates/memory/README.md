# agentd-memory

Memory — durable namespaced key/value store.

- `MemoryStore` trait.
- `RedbStore` — embedded redb file; composite key `<ns>\0<key>`; half-open prefix scans.
- `MemMemoryStore` — in-process (tests).

Backs the Lua `ctx.memory` handle; scripting gates by `memory.read:<ns>` /
`memory.write:<ns>`. Values are opaque bytes — JSON lives in scripting.

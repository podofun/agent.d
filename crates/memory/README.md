# agentd-memory

`agentd-memory` defines namespaced key-value storage for runtime state.

The `MemoryStore` trait supports get, set, delete, and prefix scan operations. `MemMemoryStore` is an in-memory implementation for tests and temporary runtimes. `RedbStore` provides persistent storage with the `redb` database.

Namespaces and keys cannot contain NUL bytes.

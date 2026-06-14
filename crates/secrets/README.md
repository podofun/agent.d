# agentd-secrets

Auth — credential store.

- `SecretStore` trait.
- `MemoryStore` — in-process (tests).
- `KeyringStore` — OS-native via `keyring-core` (libsecret on Linux, Keychain on macOS, Credential Manager on Windows).

Values are zeroized on drop. Backs the Lua `ctx.secret.*` surface; scripting gates by `secret:<key>`.

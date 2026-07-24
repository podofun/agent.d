# agentd-secrets

`agentd-secrets` defines storage for credentials and other sensitive values.

The `SecretStore` trait supports get, set, delete, and list operations. `MemoryStore` is for tests and temporary runtimes. `KeyringStore` uses the operating-system credential store and tracks names written through the current store instance.

Secret values must not be written to configuration files, logs, or traces.

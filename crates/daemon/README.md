# daemon

This crate builds the `agentd` binary — the daemon that loads your Lua components and serves the HTTP + WebSocket API.

- Config precedence: CLI flags > env vars > `config.toml` > `RUST_LOG` (log level only) > built-in default.
- Evaluates `init.lua` as the sole entry point (`runtime.init` / `--init` / `AGENTD_INIT`).
- Secrets are stored in the OS keyring; durable memory in an embedded database; built-in providers: `anthropic` (API) and `anthropic-cli`.
- Mints and `0600`-writes the public + admin tokens if unset; applies package grants before loading `grants.toml`.
- Console logging defaults to warnings/errors plus one compact startup banner. Use `AGENTD_LOG=debug` for startup detail.

```bash
agentd --init ./examples/init.lua
```

See the [documentation](../../docs/v0/) for configuration, the Lua API, and security.

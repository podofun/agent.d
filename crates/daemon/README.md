# daemon (`agentd` bin)

Runtime host + config. The binary; root of the dependency graph. No business logic
beyond config.

- `config` module — clap CLI + XDG resolution (`Cli`, `Config::resolve`). Precedence: CLI > env > `config.toml` > `RUST_LOG` (log only) > built-in default.
- Wires config → scripting → executor → api.
- Evaluates `init.lua` as the sole entry point (`runtime.init` / `--init` / `AGENTD_INIT`).
- Defaults: secrets = `KeyringStore`, memory = `RedbStore`, providers = `{ anthropic: ClaudeApiProvider, anthropic-cli: ClaudeCliProvider }`.
- Mints + `0600`-writes the public + admin tokens if unset; runs package grant desugaring before loading `grants.toml`.
- Console logging defaults to warnings/errors plus one compact startup banner. Use `AGENTD_LOG=debug` for startup detail.

```bash
cargo run -p daemon -- --init ./examples/init.lua
```

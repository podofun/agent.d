# daemon

`daemon` builds the `agentd` service.

At startup, it:

- Loads command-line options and TOML configuration.
- Loads grants, packages, Lua scripts, skills, runners, providers, secrets, and memory.
- Builds the executor, approval broker, service registry, and WebSocket API.
- Starts configured services and optional file watching.
- Installs or removes platform sandbox support when requested.

The daemon owns process lifecycle and composition. Policy and host operations remain in their dedicated crates.

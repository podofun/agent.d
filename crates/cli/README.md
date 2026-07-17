# cli

This crate builds the `agentctl` binary — the console client. It speaks WebSocket to the daemon.

Data-plane subcommands on `/ws`: `health`, `tools`, `call <action>`,
`runner ls|inspect|run`, `skills ls|inspect`, `services ls`, `trace [-f|-n N]`
(filesystem tail).

Control-plane: `grants listen` connects to `/control` with the admin token and
interactively answers approval requests.

Package commands (`packages install|update|ls|remove`) run locally against the
filesystem and git, not over the socket.

Base URL via `--url` or `AGENTD_URL`; the scheme is swapped to `ws://` / `wss://`.
Exit code 1 on failure.

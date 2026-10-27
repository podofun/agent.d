# Deployment

This page shows how to build and run the agent.d daemon in production: where the binaries go, how to point the daemon at your files, how to manage tokens, and how to run it under systemd or in a container.

## Build a release binary

You need Rust 1.85 or newer.

```bash
cargo build --release
```

This produces two binaries:

| Binary | Path |
|---|---|
| `agentd` | `target/release/agentd` |
| `agentctl` | `target/release/agentctl` |

Copy them to a directory on your `PATH` (for example `/usr/local/bin`).

## Pointing at real paths

For each setting, the daemon uses the first value it finds in this order: command-line flag, environment variable, `config.toml`, built-in default.

| Flag | Env var | XDG default |
|---|---|---|
| `--config <path>` | `AGENTD_CONFIG` | `$XDG_CONFIG_HOME/agentd/config.toml` |
| `--init <path>` | `AGENTD_INIT` | `$XDG_CONFIG_HOME/agentd/init.lua` |
| `--grants <path>` | `AGENTD_GRANTS` | `$XDG_CONFIG_HOME/agentd/grants.toml` |

The old names `--grants-file` and `AGENTD_GRANTS_FILE` (and `--trace-file` and `AGENTD_TRACE_FILE`) still work for one more release, but they are deprecated.

On most Linux systems `$XDG_CONFIG_HOME` is `~/.config`. If you place your files there, the defaults work. For other locations, use the flags or environment variables.

::: warning
A malformed `config.toml` stops the daemon from starting, so check the TOML syntax before you deploy a config change. A bad `--init` path or a Lua syntax error in the entry file also stops startup.
:::

## Bind address

The default bind address is `127.0.0.1:7777`. Keep it on localhost in production and terminate TLS upstream (see [Reverse proxy / TLS](#reverse-proxy-tls)).

To change the address:

```bash
agentd --addr 127.0.0.1:8080
# or
export AGENTD_ADDR=127.0.0.1:8080
```

You can also set it in `config.toml`:

```toml
[daemon]
addr = "127.0.0.1:8080"
```

## Token management

The daemon checks a bearer token on `/ws`, where clients connect, and a separate one on `/control`, where approvers and administrators connect.

**Generated tokens (default):** if you do not set a token, the daemon creates one on first run and writes it to:

| Endpoint | Token file |
|---|---|
| `/ws` | `$XDG_STATE_HOME/agentd/token` |
| `/control` | `$XDG_STATE_HOME/agentd/admin-token` |

Both files are written with mode `0600`. `agentctl` reads them on its own for local use.

**Fixed tokens:** set the token with a flag or environment variable. This helps when you inject secrets from a vault:

```bash
agentd \
  --token  "$AGENTD_WS_TOKEN" \
  --admin-token "$AGENTD_CTRL_TOKEN"
```

Or via env:

```bash
export AGENTD_TOKEN=<ws-token>
export AGENTD_ADMIN_TOKEN=<ctrl-token>
```

::: warning
Keep `--no-auth` and `AGENTD_NO_AUTH` off in production unless the socket is fully isolated. With auth disabled, any process that can reach the port can call any action. It is also an [admin connection](/v0/security/approvals#admin-connections), so it can make approval looser for its runs and see admin sessions.
:::

`GET /health` returns `ok` and needs no token. Use it as a liveness probe.

## Sample systemd unit

```ini
[Unit]
Description=agent.d daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/agentd \
  --init /etc/agentd/init.lua \
  --grants /etc/agentd/grants.toml \
  --config /etc/agentd/config.toml
Environment=AGENTD_TOKEN=<your-ws-token>
Environment=AGENTD_ADMIN_TOKEN=<your-ctrl-token>
Environment=AGENTD_LOG=info
# Keep tokens out of the unit file when possible — use EnvironmentFile instead:
# EnvironmentFile=/etc/agentd/secrets.env
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

::: tip
Use `EnvironmentFile=/etc/agentd/secrets.env` (mode `0600`, owned by the service user) to keep tokens out of the unit file and out of `systemctl show` output.
:::

## Running in a container

The repository contains a `Dockerfile` and a `docker-compose.yml` at the root. To start the daemon in Docker:

```bash
docker compose up --build
```

This command builds the image, mounts the example userland from `examples/docker/`, and serves the daemon on port 7777. To deploy your own agent, replace the mounted directory with your own `config.toml`, `init.lua`, and `grants.toml`.

The image differs from a host install in two ways.

**It listens on all interfaces.** On a host, the daemon defaults to `127.0.0.1:7777`, which is not reachable from outside a container. The image sets `AGENTD_ADDR=0.0.0.0:7777`, and the container boundary takes the place of localhost. Publish the port with care, and keep `/ws` and `/control` behind your proxy, as in the section below.

**It reads secrets from the environment.** Containers have no keyring service, so the image sets `AGENTD_SECRETS=env`. A variable named `AGENTD_SECRET_ANTHROPIC_API_KEY` becomes the secret `anthropic_api_key`. To keep values out of `docker inspect`, set `AGENTD_SECRETS=dir:/run/secrets`, and the daemon reads one file per secret. Docker secrets and Kubernetes secret volume mounts have this same shape. Both backends are read-only. `agentctl secret set` applies to the keyring on a host.

State lives on one volume at `/var/lib/agentd`. This volume holds the memory database, the trace log, and the generated tokens. The memory database accepts one writer at a time. Give each container its own volume. A second daemon on the same volume will refuse to start.

The image contains a health check. It probes `GET /health`, and Docker reports the container healthy after the daemon answers. In Kubernetes, point your liveness and readiness probes at the same path.

## Reverse proxy / TLS

The daemon speaks plain HTTP and WebSocket on localhost, without TLS. When external clients need `wss://` or `https://`, put a reverse proxy (nginx, Caddy, or similar) in front of it.

Proxy only the paths the deployment uses:

| Path | Protocol | Notes |
|---|---|---|
| `/health` | HTTP GET | Liveness probe, no auth |
| `/ws` | WebSocket | Client connections |
| `/control` | WebSocket | Approvers and administrators |
| `/webhooks/<name>` | HTTP POST | Configured signed webhook route |

::: warning
Expose `/control` only to trusted networks, or add authentication at the proxy. A client on `/control` can approve or deny privileged actions.
:::

Example nginx snippet (proxying to the local daemon):

```nginx
location /ws {
    proxy_pass http://127.0.0.1:7777;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
}

location /control {
    proxy_pass http://127.0.0.1:7777;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
}

location /health {
    proxy_pass http://127.0.0.1:7777;
}

location /webhooks/ {
    proxy_pass http://127.0.0.1:7777;
}
```

## See also

- [Observability](/v0/operations/observability): trace log and log-level configuration
- [Troubleshooting](/v0/operations/troubleshooting): common startup and runtime issues
- [Reference: configuration](/v0/reference/configuration): full config.toml schema
- [Security: grants](/v0/security/grants): grants.toml reference

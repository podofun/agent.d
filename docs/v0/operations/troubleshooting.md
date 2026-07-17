# Troubleshooting

Common problems running agent.d in production and how to fix them.

## Daemon won't start

**Symptom:** The `agentd` process exits immediately after launch.

| Cause | Fix |
|---|---|
| Bad `--init` path | Verify the path exists and is readable. The default is `$XDG_CONFIG_HOME/agentd/init.lua`. |
| Lua syntax error in entry file | Run `lua -e 'dofile("init.lua")'` (or equivalent) locally to surface parse errors. Check the daemon's stderr for the error line. |
| Malformed `config.toml` | `config.toml` is parsed at startup and any TOML error is fatal. Validate with `cat config.toml | python3 -c "import tomllib,sys; tomllib.loads(sys.stdin.read())"` or a TOML linter. |
| Bad `--grants` path | Verify the path exists. An unreadable grants file prevents the permission engine from initialising. |
| Port already in use | Another process is bound to `127.0.0.1:7777`. Change the address with `--addr` or stop the conflicting process. |

Check stderr output — startup errors are printed there before the process exits.

## `agentctl` can't connect

**Symptom:** `agentctl health` (or any subcommand) times out or returns a connection error.

| Cause | Fix |
|---|---|
| Wrong URL | `agentctl` defaults to `http://127.0.0.1:7777`. If the daemon binds elsewhere, pass `--url` or set `AGENTD_URL`. |
| Token mismatch | Auto-minted tokens are written to `$XDG_STATE_HOME/agentd/token` (ws) and `$XDG_STATE_HOME/agentd/admin-token` (control). `agentctl` reads these automatically for local use. If the daemon was started with an explicit `--token`, set the same value in `AGENTD_TOKEN` or pass it directly. |
| Daemon not running | Confirm the process is up: `systemctl status agentd` or `ps aux | grep agentd`. |

::: tip
For quick local debugging, start the daemon with `--no-auth` (`AGENTD_NO_AUTH=1`) to skip token checks entirely. Never use this in production.
:::

## Action denied

**Symptom:** An `agentctl call` or a `ctx.call()` inside a Lua handler returns a permission error.

The permission engine is default-deny across five layers. A call is denied when any layer rejects it.

**Diagnose:** Run the approval console and retry the call:

```bash
agentctl grants listen
```

The console prints the missing permission slugs and the caller identity. From there:

1. **Missing tool/service grant** — add the slug to `[tool.<name>]` or `[service.<name>]` in `grants.toml`:
   ```toml
   [tool.git]
   granted = ["shell.exec:git"]
   ```
2. **Runner or interface allowlist** — the action is not in `runner.allowed_actions` or `interface.allowed_actions`. Add it, or remove the allowlist restriction if intentional.
3. **Policy hard-denial** — if `deny_actions` or `deny_permissions` in `[policy]` matches the call, it will never be escalated to approval. Remove the policy entry to allow the call.

## Confirm actions rejected by `ctx.call`

**Symptom:** Calling an action with `confirm = true` via `ctx.call()` inside a handler returns a denial immediately, even when an operator is connected.

This is expected behaviour. `ctx.call()` from inside another action does **not** escalate `confirm = true` actions to the approval plane — they are rejected outright. Use interactive approval only from a direct client call, not from within Lua handler code.

## Provider / model errors

**Symptom:** A runner call returns an error about a missing credential or the provider being unavailable.

| Cause | Fix |
|---|---|
| Missing API key | The `anthropic` and `openai` providers read credentials from the OS keyring via the secret store. Seed the key with `agentctl secret set` (e.g. `echo "$KEY" | agentctl secret set anthropic_api_key`) — see [Providers: credentials](/v0/providers/credentials). |
| Missing `ai:` grant | The action or service needs an `ai:<provider>` grant in `grants.toml`. Example: `granted = ["ai:anthropic"]`. |
| Wrong model string | Model strings must use the `"<provider>/<model_id>"` format (e.g. `anthropic/claude-opus-4-7`). Omitting the prefix defaults to `anthropic`. |
| CLI backend not on PATH | `anthropic-cli` requires `claude` to be installed and executable; `codex` / `openai-cli` require `codex`. |

## Services flapping

**Symptom:** `agentctl services ls` shows a service repeatedly cycling between states, or `last_error` is populated.

1. Read `last_error` from `agentctl services ls` to find the root cause.
2. Check the trace log: `agentctl trace -f` or `agentctl trace -n 100`.
3. Common causes:
   - Missing grant (`net:`, `secret:`, `memory.*`) — add to `[service.<name>]` in `grants.toml`.
   - External dependency unavailable (network, credential not set).
   - Lua logic error in the service body.
4. After fixing, the service will restart automatically if registered with `restart = "always"` or `restart = "on_failure"`.

## Hot reload not firing

**Symptom:** You changed `init.lua` (or an imported file, a skill `.md`, or `grants.toml`) but the daemon did not reload.

- Confirm the daemon was started with `--watch` (`AGENTD_WATCH=1`). Hot reload is a **development feature** and is off by default.
- Check that the modified file is one the watcher tracks: `init.lua`, every file reachable via `import()`, loaded skill `.md` sources, and `grants.toml`.
- Changes to files **not** reachable from the entry file are not watched. If you added a new `import()` path, you need to restart the daemon once for the watcher to pick it up.

::: warning
Do not run `--watch` in production. It rebuilds the entire runtime in-place on every file change, which is useful during development but introduces unnecessary churn in a live deployment.
:::

## See also

- [Deployment](/v0/operations/deployment) — startup flags, token management, systemd unit
- [Observability](/v0/operations/observability) — trace log, log levels, service health
- [Security: grants](/v0/security/grants) — grants.toml schema and permission slugs
- [Security: approvals](/v0/security/approvals) — interactive approval flow

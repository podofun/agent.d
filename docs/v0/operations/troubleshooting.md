# Troubleshooting

This page lists problems you are likely to meet while running agent.d, what causes them, and how to fix each one. Most errors already say what went wrong, so start by reading the full message.

## Daemon won't start

**Symptom:** The `agentd` process exits immediately after launch.

| Cause | Fix |
|---|---|
| Bad `--init` path | Verify the path exists and is readable. The default is `$XDG_CONFIG_HOME/agentd/init.lua`. |
| Lua syntax error in entry file | The daemon prints the error on stderr. You can also run `lua -e 'dofile("init.lua")'` (or equivalent) locally to find parse errors. |
| Malformed `config.toml` | `config.toml` is read at startup, and any TOML error stops the daemon. Check it with `cat config.toml | python3 -c "import tomllib,sys; tomllib.loads(sys.stdin.read())"` or a TOML linter. |
| Bad `--grants` path | Verify the path exists. The daemon cannot start without a readable grants file. |
| Port already in use | Another process is bound to `127.0.0.1:7777`. Change the address with `--addr` or stop the conflicting process. |

Startup errors are printed on stderr before the process exits, so check there first.

## `agentctl` can't connect

**Symptom:** `agentctl health` (or any subcommand) times out or returns a connection error.

| Cause | Fix |
|---|---|
| Wrong URL | `agentctl` defaults to `http://127.0.0.1:7777`. If the daemon binds elsewhere, pass `--url` or set `AGENTD_URL`. |
| Token mismatch | Tokens the daemon creates for you are written to `$XDG_STATE_HOME/agentd/token` (ws) and `$XDG_STATE_HOME/agentd/admin-token` (control). `agentctl` reads these on its own for local use. If the daemon was started with an explicit `--token`, set the same value in `AGENTD_TOKEN` or pass it directly. |
| Daemon not running | Confirm the process is up: `systemctl status agentd` or `ps aux | grep agentd`. |

::: tip
For quick local debugging, start the daemon with `--no-auth` (`AGENTD_NO_AUTH=1`) to skip token checks. Keep it off in production.
:::

## Action denied

**Symptom:** An `agentctl call` or a `ctx.call()` inside a Lua handler returns a permission error.

agent.d is default-deny. A call fails when a denial matches it, or when a question about it does not get an allow.

**Diagnose:** Run the approval console and retry the call:

```bash
agentctl grants listen
```

The console prints the missing permission slugs and the caller identity. From there:

1. **Missing tool or service grant**: add the slug to `[tool.<name>]` or `[service.<name>]` in `grants.toml`:
   ```toml
   [tool.git]
   granted = ["shell.exec:git"]
   ```
2. **Runner or interface allowlist**: the action is not in `runner.allowed_actions` or `interface.allowed_actions`. Add it, or remove the allowlist if you no longer want the limit.
3. **A denial**: `[denial].actions` or `[denial].permissions` matches the call, and denied calls are refused without a question. Remove the entry to allow the call.
4. **The run's approval is `nobody`**: the error says that the call "was refused without asking". The run can use only what is granted. Add the grant, or start the run with a looser setting from an admin connection.
5. **The classifier refused**: the error starts with "The classifier refused" and gives its reason. Change the classifier's instructions, or add the grant.

## A shell command is refused because of a denial

**Symptom:** A `ctx.shell` call fails with "this shell command could read what ... denies in grants.toml".

The command's sandbox opens a folder that contains a denied file, or allows a host that you denied. A sandbox cannot hide one file inside a folder it opens, so agent.d refuses the command. Move the denied file out of the granted folders, or grant narrower folders and hosts. See [What a denial guarantees](/v0/security/denials#fix-a-refused-shell-command).

## `grants.toml` does not load

**Symptom:** The daemon does not start, or a reload with `--watch` prints an error about `grants.toml`.

agent.d refuses a file with a mistake, so you find out about a broken rule at load time. The error names the line to fix. Common causes are a misspelled key, a `*` in the middle of a value, a relative file path in `[denial]`, and an old `[policy]` section. See [The file is strict](/v0/security/grants#the-file-is-strict).

## Confirm actions rejected by `ctx.call`

**Symptom:** Calling an action with `confirm = true` via `ctx.call()` inside a handler returns a denial immediately, even when an operator is connected.

This is expected. When one action calls a `confirm = true` action with `ctx.call()`, agent.d refuses the call without sending a question. Call confirm actions directly from a client, where an approver can answer.

## Provider and model errors

**Symptom:** A runner call returns an error about a missing credential or the provider being unavailable.

| Cause | Fix |
|---|---|
| Missing API key | The `anthropic` and `openai` providers read credentials from the OS keyring via the secret store. Store the key with `agentctl secret set` (for example `echo "$KEY" | agentctl secret set anthropic_api_key`). See [Providers: credentials](/v0/providers/credentials). |
| Missing `ai:` grant | The action or service needs an `ai:<provider>` grant in `grants.toml`. Example: `granted = ["ai:anthropic"]`. |
| Wrong model string | Model strings must use the `"<provider>/<model_id>"` format (e.g. `anthropic/claude-opus-4-7`). Without a prefix, the provider is `anthropic`. |
| CLI backend not on PATH | `anthropic-cli` needs `claude` installed and on `PATH`. `codex` and `openai-cli` need `codex`. |

## Services flapping

**Symptom:** `agentctl services ls` shows a service repeatedly cycling between states, or `last_error` is populated.

1. Read `last_error` from `agentctl services ls` to find the root cause.
2. Check the trace log: `agentctl trace -f` or `agentctl trace -n 100`.
3. Common causes:
   - Missing grant (`net:`, `secret:`, `memory.*`): add it to `[service.<name>]` in `grants.toml`.
   - External dependency unavailable (network, credential not set).
   - Lua logic error in the service body.
4. After the fix, a service registered with `restart = "always"` or `restart = "on_failure"` restarts on its own.

## Hot reload not firing

**Symptom:** You changed `init.lua` (or an imported file, a skill `.md`, or `grants.toml`) but the daemon did not reload.

- Confirm the daemon was started with `--watch` (`AGENTD_WATCH=1`). Hot reload is meant for development and is off by default.
- Check that the modified file is one the watcher tracks: `init.lua`, every file reachable via `import()`, loaded skill `.md` sources, and `grants.toml`.
- The watcher follows only files reachable from the entry file. If you added a new `import()` path, restart the daemon once so the watcher picks it up.

::: warning
Leave `--watch` off in production. It rebuilds the whole configuration on every file change, which helps during development and adds needless churn to a live deployment.
:::

## See also

- [Deployment](/v0/operations/deployment): startup flags, token management, systemd unit
- [Observability](/v0/operations/observability): trace log, log levels, service health
- [Security: grants](/v0/security/grants): grants.toml schema and permission slugs
- [Security: approvals](/v0/security/approvals): interactive approval flow

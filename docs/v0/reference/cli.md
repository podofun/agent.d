# agentctl CLI Reference

`agentctl` is the console client for agent.d. It speaks WebSocket to the daemon for every command except `health`, which uses plain HTTP.

## Global flags

| Flag | Env | Default | Description |
|---|---|---|---|
| `--url <URL>` | `AGENTD_URL` | `http://127.0.0.1:7777` | Daemon base URL |
| `--timeout <ms>` | — | `30000` | Connect timeout in milliseconds |

**Token resolution.** For `/ws` commands, `agentctl` reads the bearer token from `AGENTD_TOKEN` first, then falls back to `$XDG_STATE_HOME/agentd/token` (the file the daemon writes at startup). If neither exists the daemon must be running with `--no-auth`. For `grants listen` the same logic applies to `AGENTD_ADMIN_TOKEN` / `$XDG_STATE_HOME/agentd/admin-token` on the `/control` plane.

**Transport.** Every command except `health` opens a WebSocket connection to `/ws`, sends one JSON envelope, and exits when the response arrives. `health` uses `GET /health` (HTTP).

---

## Commands

### `agentctl health`

Check daemon liveness via `GET /health`. Returns `ok` when the daemon is up.

```bash
agentctl health
```

---

### `agentctl tools`

List all registered action names.

```bash
agentctl tools
```

Each name is printed one per line in `tool.action` form.

---

### `agentctl call`

Invoke an action and print the result.

```bash
agentctl call <action> [--json '<json>'] [-d key=value]... [--result-only] [--compact]
```

| Flag | Description |
|---|---|
| `--json '<json>'` | Pass arguments as a raw JSON string |
| `-d key=value` | Pass a single argument; value is parsed as JSON, falls back to string |
| `--result-only` | Print only the `result` field, not the full envelope |
| `--compact` | Print JSON on one line instead of pretty-printed |

::: warning Mutually exclusive
`--json` and `-d` cannot be used together.
:::

**Output shape** (without `--result-only`):

```json
{
  "result": { ... },
  "duration_ms": 42
}
```

**Examples:**

```bash
# No arguments
agentctl call git.status

# Key=value arguments
agentctl call git.diff -d path=src/

# JSON argument blob
agentctl call git.diff --json '{"path":"src/"}'

# Only print the action's return value
agentctl call git.status --result-only

# Machine-readable one-liner
agentctl call git.status --result-only --compact
```

---

### `agentctl runner ls`

List registered runners with their configured model.

```bash
agentctl runner ls
```

Output is tab-separated `name<TAB>model` lines.

---

### `agentctl runner inspect <name>`

Print the full composition of a runner (merged system prompt, resolved skills, allowed actions).

```bash
agentctl runner inspect backend_reviewer
```

---

### `agentctl runner run <name> "<prompt>"`

Run a runner with a text prompt and print the result.

```bash
agentctl runner run <name> "<prompt>" [--text-only]
```

| Flag | Description |
|---|---|
| `--text-only` | Print only the `text` field of the response |

**Output shape** (without `--text-only`):

```json
{
  "text": "...",
  "provider": "anthropic",
  "model": "claude-opus-4-7",
  "stop_reason": "end_turn"
}
```

**Example:**

```bash
agentctl runner run backend_reviewer "Review the latest diff" --text-only
```

---

### `agentctl skills ls`

List registered skills with their description.

```bash
agentctl skills ls
```

Output is tab-separated `name<TAB>description` lines.

---

### `agentctl skills inspect <name>`

Print the full definition of a skill.

```bash
agentctl skills inspect reviewer
```

---

### `agentctl services ls`

List running services with their state and any last error.

```bash
agentctl services ls
```

Output is tab-separated `name<TAB>state[<TAB>last_error]` lines.

---

### `agentctl grants listen`

Connect to the `/control` plane and interactively answer permission-approval requests. Runs until the connection closes or Ctrl-C. On each request you are prompted to allow once (`o`), allow forever (`f`), or deny (`d`); anything else or EOF defaults to `deny`.

```bash
agentctl grants listen
```

Uses `AGENTD_ADMIN_TOKEN` / `$XDG_STATE_HOME/agentd/admin-token` for authentication.

---

### `agentctl packages ls`

List installed packages, their pinned commit, ref, and whether an update is available.

```bash
agentctl packages ls
```

::: info Local operation
`packages` commands are local filesystem + git operations — they do not talk to a running daemon.
:::

---

### `agentctl packages install <git-url>`

Clone a package from a git URL, read its `package.toml` manifest, and register it in the local package index.

```bash
agentctl packages install <git-url> [--ref <ref>]
```

| Flag | Description |
|---|---|
| `--ref <ref>` | Git ref (branch, tag, or SHA) to pin |

After install, if the package declares permissions you are shown the slugs and told to add `[package.<name>] trusted = true` to `grants.toml` before they take effect.

```bash
agentctl packages install https://github.com/example/acme-tools
agentctl packages install https://github.com/example/acme-tools --ref v1.2.0
```

---

### `agentctl packages update <name>`

Re-pull the package and update its pinned commit in the index.

```bash
agentctl packages update acme-tools
```

---

### `agentctl packages remove <name>`

Delete the package directory and remove it from the index.

```bash
agentctl packages remove acme-tools
```

---

### `agentctl types [dir]`

Fetch live action/runner/skill names from the daemon and write LuaLS type stubs into the project's `.luals/` directory.

```bash
agentctl types [dir]
```

- `dir` defaults to the current directory (the folder containing `init.lua`).
- Writes `.luals/agentd.lua`, `.luals/project.lua`, and merges `.luarc.json`.
- This is the same regeneration that `--watch` triggers automatically on reload.

```bash
# Current project
agentctl types

# Specific project directory
agentctl types ~/projects/my-agent
```

---

### `agentctl trace`

Tail the JSONL trace file.

```bash
agentctl trace [--file <path>] [-f] [-n <N>]
```

| Flag | Short | Default | Description |
|---|---|---|---|
| `--file <path>` | | `$XDG_STATE_HOME/agentd/trace.jsonl` | Trace file to read |
| `--follow` | `-f` | off | Stream new lines as they are appended |
| `--lines <N>` | `-n` | `20` | Number of tail lines to show |

```bash
# Last 20 lines
agentctl trace

# Follow with 50-line history
agentctl trace -f -n 50

# Alternate file
agentctl trace --file /tmp/my-trace.jsonl -f
```

---

## See also

- [WebSocket protocol](/v0/reference/protocol)
- [Configuration reference](/v0/reference/configuration)
- [Permissions & grants](/v0/security/grants)
- [Observability](/v0/operations/observability)

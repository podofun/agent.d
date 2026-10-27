# agentctl CLI Reference

`agentctl` is the console client for agent.d. It speaks WebSocket to the daemon for every command except `health`, which uses plain HTTP.

## Global flags

| Flag | Env | Default | Description |
|---|---|---|---|
| `--url <URL>` (`-u`) | `AGENTD_URL` | `http://127.0.0.1:7777` | Daemon base URL |
| `--timeout <ms>` | | `30000` | Connect timeout in milliseconds |

**Token resolution.** For `/ws` commands, `agentctl` uses the first token it finds, in this order: `AGENTD_TOKEN`, then the admin token (`AGENTD_ADMIN_TOKEN` or `$XDG_STATE_HOME/agentd/admin-token`), then `$XDG_STATE_HOME/agentd/token`. The daemon writes both files at startup. On your own machine, every command therefore connects as an [admin connection](/v0/security/approvals#admin-connections) and sees the same chats. To connect as an ordinary client, set `AGENTD_TOKEN`. If no token exists, the daemon must be running with `--no-auth`. `grants listen` connects to the `/control` plane with `AGENTD_ADMIN_TOKEN` or `$XDG_STATE_HOME/agentd/admin-token`.

**Transport.** Every command except `health` opens a WebSocket connection to `/ws`, sends one JSON envelope, and exits when the response arrives. `health` uses `GET /health` (HTTP).

---

## Commands

**Noun aliases.** Command nouns are singular, with plural (and short) forms accepted as aliases:

| Canonical | Aliases |
|---|---|
| `runner` | `runners` |
| `skill` | `skills` |
| `service` | `services`, `svc` |
| `package` | `packages`, `pkg` |
| `secret` | `secrets` |

`agentctl pkg ls` and `agentctl runners ls` are equivalent to `agentctl package ls` and `agentctl runner ls`.

### `agentctl chat`

`agentctl chat` opens a chat with your runners in the terminal. Use it to talk to an agent, answer its permission requests and pick up earlier conversations.

```bash
agentctl chat
```

If you have one runner, chat selects it for you. Otherwise, pick one with `/runner`. You can use `/runner` and `/open` at any time to switch runners or reopen a past chat. To start with a choice already made, pass `--runner <name>` or `--session <id>`.

#### Typing and sending

Enter sends your message and Shift+Enter adds a new line. You can keep typing while the agent works: when you press Enter, your message waits in the conversation, marked "queued", and chat sends it when the current reply finishes. If the reply fails or you cancel it, queued messages go back into your draft. Ctrl+C stops a running reply. When nothing is running and the draft is empty, press Ctrl+C twice to leave chat.

When an agent calls several actions in a row, chat shows them as one line, for example "Used 14 actions and 1 tool". Click it, or press Ctrl+E, to see each call.

Code in replies is highlighted by language with your terminal's own colors, so it reads well on light and dark themes. To copy text from the conversation, drag with the mouse to select it and press Ctrl+C.

Type `/` to see what you can do:

| Command | Description |
|---|---|
| `/runner <name>` | Switch to another runner |
| `/new` or `/clear` | Start a new chat |
| `/approval <mode>` | Choose who answers requests: `human`, `classifier`, `nobody`, or `default` |
| `/open <id>` | Reopen a past chat |
| `/rename <name>` | Name the current chat so it is easy to find later |
| `/call <action> [json]` | Run an action |
| `/help` | Show every command and shortcut |
| `/quit` | Leave chat |

#### Approving requests

When a runner needs a permission it has not been granted, chat shows the request in the conversation, and you allow or deny it there.

Each chat has an approval mode that decides who answers these requests. Press Shift+Tab to cycle through the three modes, or use `/approval`:

| Mode | What happens |
|---|---|
| `human` (ask me) | Chat asks you about each request. |
| `classifier` (auto) | The classifier answers each request, and asks you only when it is unsure. You need a classifier in `grants.toml`. See [The classifier](/v0/security/approvals#the-classifier). |
| `nobody` (no approvals) | Anything that is not granted is refused, and nobody is asked. |

The header shows the current mode. A chat keeps its mode when you reopen it, and `/approval default` goes back to the runner's default.

#### Folders

A chat works in the folder you start `agentctl chat` from. The first time you use a folder, agent.d asks whether to trust it. "Allow once" trusts it for this chat, and "Always trust" saves it in `grants.toml`. If you decline, the chat starts without a folder and tells you so.

#### Finding chats, runners and actions

Press Ctrl+P to open the browser, a search across your chats, runners, skills, actions and services. Start typing to filter the list. The left and right arrows (or Tab) move between sections, and the up and down arrows move through the results. Enter opens the selected item: it resumes a chat, switches to a runner, shows a skill or puts an action in the composer. The panel beside the list shows details of the selected item, including how many tokens a chat has used. Esc clears the search, and a second Esc returns to the chat.

---

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
agentctl call <action> [-j '<json>'] [-d key=value]... [-r] [--compact]
```

| Flag | Description |
|---|---|
| `--json '<json>'` (`-j`) | Pass arguments as a raw JSON string |
| `-d key=value` (`--data`) | Pass a single argument; value is parsed as JSON, falls back to string |
| `--result-only` (`-r`) | Print only the `result` field of the envelope |
| `--compact` | Print JSON on one line |

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
agentctl call git.status -r

# Machine-readable one-liner
agentctl call git.status -r --compact
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

```text
agentctl runner run <name> "<prompt>" [--session <id>] [--user <user>] [--text-only] [--stream]
```

| Flag | Description |
|---|---|
| `--session <id>` | Continue a chat session from `agentctl session new`. The daemon loads its history and stores this exchange |
| `--user <user>` | Act as this end user. Needed for sessions created with `--user` |
| `--text-only` | Print only the `text` field of the response |
| `--stream` | Print the response as it is generated; tool calls show as dim `[tool: name]` markers on stderr |

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

### `agentctl session new`

Creates a chat session and prints its id. A session keeps a conversation's history in the daemon, so later runs in it remember earlier messages. Use `--label` to give it a name you can find it by later, such as a chat id or ticket number. Labels are unique per interface across all users.

```text
agentctl session new [--label <label>] [--user <user>] [--cwd <folder>]
```

```bash
# Requires the support runner and provider grant from the session example.
SID=$(agentctl session new --label demo --user fathi)
agentctl runner run support "My name is Fathi." --session "$SID" --user fathi --text-only
agentctl runner run support "What is my name?" --session "$SID" --user fathi --text-only
```

First register the `support` runner and configure its provider and grants as shown in [Running a session](/v0/reference/ctx/sessions#running-a-session). Use a new label each time you run this example.

A session belongs to the interface whose token `agentctl` connects with (`AGENTD_TOKEN`), and to the `--user` it was created with. Pass the same `--user` on every later command; with a different user, or none, the daemon treats the session as missing. See [who can see a session](/v0/reference/ctx/sessions#who-can-see-a-session).

---

### `agentctl session ls [--user <user>] [--limit <n>]`

Lists sessions from newest to oldest, one line each, with the session's id, label, runner and stored message count. `--limit` defaults to 50. The list shows only sessions visible to your token and `--user`.

---

### `agentctl session show <id-or-label> [--user <user>]`

Prints a session with every stored turn. A 36-character UUID is looked up as an id, and anything else as a label.

---

### `agentctl session rm <id> [--user <user>]`

Deletes a session and its turns.

---

### `agentctl skill ls`

List registered skills with their description.

```bash
agentctl skill ls
```

Output is tab-separated `name<TAB>description` lines.

---

### `agentctl skill inspect <name>`

Print the full definition of a skill.

```bash
agentctl skill inspect reviewer
```

---

### `agentctl service ls`

List running services with their state and any last error.

```bash
agentctl service ls
```

Output is tab-separated `name<TAB>state[<TAB>last_error]` lines.

---

### `agentctl grants listen`

Use `grants listen` to answer permission requests from a terminal while agents run elsewhere. It connects to the `/control` plane and prompts you for each request: allow once (`o`), allow forever (`f`) or deny (`d`). Any other answer, or end of input, denies the request. It runs until the connection closes or you press Ctrl+C.

```bash
agentctl grants listen
```

It authenticates with `AGENTD_ADMIN_TOKEN` or `$XDG_STATE_HOME/agentd/admin-token`.

---

### `agentctl secret set`

Store a provider API key (or any secret) in the OS keyring, under the same `agentd` service the daemon reads. Providers read the keyring at call time, so a running daemon uses the new value on its next call without a restart.

```bash
agentctl secret set <name> [value]
```

The value is optional. When you leave it off, `agentctl` reads it from stdin. This keeps the key out of your shell history, and is the form we recommend:

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

On success:

```
stored `anthropic_api_key` — available to the daemon immediately
```

::: info Local operation
`secret` commands write to the OS keyring directly. They work whether or not the daemon is running.
:::

---

### `agentctl secret unset <name>`

Remove a secret from the keyring. `rm` is an alias for `unset`.

```bash
agentctl secret unset anthropic_api_key
agentctl secrets rm anthropic_api_key
```

---

### `agentctl secret peek <name>`

Prints a partly masked preview of a stored secret, enough to confirm which key is stored:

```bash
agentctl secret peek my_key
# sk-t************ef (24 chars)
```

Short values are fully masked. `peek` shows only this preview.

::: info No `ls` subcommand
OS keyrings cannot enumerate their entries portably, so there is no `secret ls`. Use `peek` to check whether a specific key is stored.
:::

---

### `agentctl package ls`

List installed packages, their pinned commit, ref, and whether an update is available.

```bash
agentctl package ls
```

::: info Local operation
`package` commands work on local files and git. They do not need a running daemon.
:::

---

### `agentctl package install <git-url>`

Clone a package from a git URL, read its `package.toml` manifest, and register it in the local package index.

```bash
agentctl package install <git-url> [--ref <ref>]
```

| Flag | Description |
|---|---|
| `--ref <ref>` | Git ref (branch, tag, or SHA) to pin |

After install, if the package declares permissions you are shown the slugs and told to add `[package.<name>] trusted = true` to `grants.toml` before they take effect.

```bash
agentctl package install https://github.com/example/acme-tools
agentctl package install https://github.com/example/acme-tools --ref v1.2.0
```

---

### `agentctl package update <name>`

Re-pull the package and update its pinned commit in the index.

```bash
agentctl package update acme-tools
```

---

### `agentctl package remove <name>`

Delete the package directory and remove it from the index. `rm` is an alias for `remove`.

```bash
agentctl package remove acme-tools
agentctl pkg rm acme-tools
```

---

### `agentctl types [dir]`

Fetch live action, runner, and skill names from the daemon. Then, write LuaLS type stubs into the configuration directory.

```bash
agentctl types [dir]
```

- `dir` defaults to the current directory (the folder containing `init.lua`).
- Writes `.luals/agentd.lua`, `.luals/project.lua`, and merges `.luarc.json`.
- This is the same regeneration that `--watch` triggers automatically on reload.

```bash
# Current configuration directory
agentctl types

# Specified configuration directory
agentctl types /path/to/agentd-config
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

## Error output

When a command fails, `agentctl` renders the daemon's error envelope for humans:

```
Error: Could not resolve a provider for model `github/openai/gpt-4o-mini`  (no_provider)
Tip: You can configure new providers in your `config.toml`

Stack trace:
  helpers.lua:313  in structured
  init.lua:53
```

- The trailing `(code)` suffix is dimmed on a TTY.
- The `Tip:` line appears when the error code has a tip, and the `Stack trace:` section appears when the failure was raised inside a Lua script.
- With `call --compact`, errors are printed as one-line JSON: `{"code":...,"error":...,"tip":...,"trace":[...]}`.

See the [error codes table](/v0/reference/protocol#error-codes) for the full code and tip catalog.

---

## See also

- [WebSocket protocol](/v0/reference/protocol)
- [Configuration reference](/v0/reference/configuration)
- [Permissions & grants](/v0/security/grants)
- [Observability](/v0/operations/observability)

# agentctl CLI Reference

`agentctl` is the command-line tool for managing and invoking a running agent.d instance. It lets you do everyday things — call an action, run a runner, approve a permission request, store an API key, install a package — directly from your terminal, without writing any code or talking to the server API yourself.

If you have not set up a project yet, start with the [quick start guide](/v0/guide/quick-start). Everything on this page assumes a daemon is already running, except where noted.

## Global flags

These flags work with every command:

| Flag | Env | Default | Description |
|---|---|---|---|
| `-u`, `--url <URL>` | `AGENTD_URL` | `http://127.0.0.1:7777` | Address of the daemon to talk to |
| `--timeout <ms>` | — | `30000` | Connect timeout in milliseconds |

You do not need to configure authentication. When the daemon starts, it writes its access token to a well-known file, and `agentctl` picks it up from there automatically. If you need to override it — for example, to reach a daemon on another machine — set the `AGENTD_TOKEN` environment variable.

Command nouns are singular, and the plural or short forms work as aliases: `runner`/`runners`, `skill`/`skills`, `service`/`services`/`svc`, `package`/`packages`/`pkg`, `secret`/`secrets`. Writing `agentctl pkg ls` is the same as writing `agentctl package ls` — use whichever comes naturally.

## Working with actions

Actions are the individual capabilities your project exposes — things like `git.status` or `http.fetch`. See [Tools and actions](/v0/concepts/tools-and-actions) for the concept.

### `agentctl tools`

Lists the names of every action registered in your project, one per line. This is the quickest way to see what your daemon can do.

```bash
agentctl tools
```

### `agentctl call`

Invokes an action by name and prints its result. This is the command you will probably use most while building a project.

```bash
agentctl call <action> [-j '<json>'] [-d key=value]... [-r] [--compact]
```

If the action takes arguments, pass them either one at a time with `-d key=value`, or all at once as a JSON string with `-j`. The two styles cannot be mixed in one call.

```bash
agentctl call git.status                        # no arguments
agentctl call git.diff -d path=src/             # one key=value argument
agentctl call git.diff -j '{"path":"src/"}'     # arguments as JSON
```

By default the output includes the result and how long the call took:

```json
{ "result": { ... }, "duration_ms": 42 }
```

Add `-r` (`--result-only`) to print just the action's return value, and `--compact` to put everything on one line — handy when piping into other tools:

```bash
agentctl call git.status -r --compact
```

## Working with runners

Runners are your configured AI agents — a model plus a system prompt plus a set of skills. See [Runners](/v0/concepts/runners) for the concept.

### `agentctl runner ls`

Lists your runners and the model each one uses.

```bash
agentctl runner ls
```

### `agentctl runner inspect <name>`

Shows everything that makes up a runner: its merged system prompt, the skills it resolved, and the actions it is allowed to call. Useful when a runner is not behaving the way you expect and you want to see exactly what it was given.

```bash
agentctl runner inspect backend_reviewer
```

### `agentctl runner run <name> "<prompt>"`

Sends a prompt to a runner and prints the reply. This is the fastest way to try a runner out without wiring up an interface.

```bash
agentctl runner run backend_reviewer "Review the latest diff"
```

The full response includes the reply text along with which provider and model produced it. If you only want the text — for example, in a shell script — add `--text-only`:

```bash
agentctl runner run backend_reviewer "Review the latest diff" --text-only
```

## Working with skills and services

Skills are reusable instruction sets that runners compose; services are long-running background tasks your project starts. See [Skills](/v0/concepts/skills) and [Services](/v0/concepts/services).

### `agentctl skill ls`

Lists your skills with their descriptions.

```bash
agentctl skill ls
```

### `agentctl skill inspect <name>`

Shows a skill's full definition.

```bash
agentctl skill inspect reviewer
```

### `agentctl service ls`

Lists your services and the state each one is in. If a service has crashed, its last error is shown next to it — this is the first place to look when a background task goes quiet.

```bash
agentctl service ls
```

## Approving permission requests

When a runner or tool tries to do something it has not been granted — read a file outside its sandbox, call a new binary — the daemon can ask a human before deciding. This command makes your terminal that human.

### `agentctl grants listen`

Connects to the daemon and waits for permission requests. Each request shows you who is asking and what for, then prompts for a decision: press `o` to allow it once, `f` to allow it permanently (the grant is written to `grants.toml`), or `d` to deny. Anything else denies — the safe default. The command runs until you press Ctrl-C.

```bash
agentctl grants listen
```

See [Grants](/v0/security/grants) for how permissions work.

## Managing secrets

Providers read their API keys from your operating system's keyring. These commands manage those keys. They work on the keyring directly, so they do not need the daemon to be running — and if it is running, it picks up changes on its next model call.

### `agentctl secret set <name> [value]`

Stores an API key (or any other secret) under the given name.

```bash
agentctl secret set anthropic_api_key sk-ant-…
```

You can leave the value off and pipe it in instead. This keeps the key out of your shell history, and is the form we recommend:

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

### `agentctl secret unset <name>`

Removes a stored secret. `rm` works as an alias.

```bash
agentctl secret unset anthropic_api_key
```

### `agentctl secret peek <name>`

Shows a partially masked preview of a stored secret — enough to confirm which key is in there, without revealing it. Short values are masked entirely.

```bash
agentctl secret peek anthropic_api_key
# sk-a************t- (32 chars)
```

See [Credentials](/v0/providers/credentials) for the full picture of how keys and grants fit together.

## Managing packages

Packages are shareable bundles of tools and skills, installed from git repositories. These commands manage your local package library and do not need the daemon to be running. See [Packages](/v0/packages/) for the concept.

### `agentctl package ls`

Lists your installed packages, the commit each one is pinned to, and whether a newer version is available.

```bash
agentctl package ls
```

### `agentctl package install <git-url>`

Downloads a package from a git URL and registers it locally. Pass `--ref` to pin a specific branch, tag, or commit.

```bash
agentctl package install https://github.com/example/acme-tools
agentctl package install https://github.com/example/acme-tools --ref v1.2.0
```

If the package asks for permissions, the install output lists them along with the exact `grants.toml` line you need to add before they take effect — nothing is granted automatically.

### `agentctl package update <name>`

Pulls the latest version of a package and updates its pin.

```bash
agentctl package update acme-tools
```

### `agentctl package remove <name>`

Deletes a package and forgets about it. `rm` works as an alias.

```bash
agentctl package remove acme-tools
```

## Development helpers

### `agentctl health`

Checks that the daemon is up and reachable. Prints `ok` if it is.

```bash
agentctl health
```

### `agentctl types [dir]`

Generates editor type stubs for your project, so your editor can autocomplete action, runner, and skill names and check your Lua against the real API. It asks the running daemon for the live names and writes the stubs into your project's `.luals/` directory.

```bash
agentctl types                      # current project
agentctl types ~/projects/my-agent  # a specific project
```

The directory defaults to wherever you run the command — it should be the folder containing your `init.lua`. If your daemon runs with `agentd --watch`, the stubs regenerate automatically on every reload and you rarely need this command at all.

### `agentctl trace`

Shows the daemon's trace log — a structured record of every action call, runner turn, and permission decision. Useful for understanding what actually happened during a run.

```bash
agentctl trace              # the last 20 entries
agentctl trace -f           # keep following as new entries arrive
agentctl trace -f -n 50     # follow, starting with the last 50
```

Pass `--file <path>` to read a trace file other than the default. See [Observability](/v0/operations/observability) for how tracing works.

## When something goes wrong

When a command fails, `agentctl` explains what happened in plain language, and where possible tells you how to fix it:

```
Error: Provider `github` is not configured — store the API key in the `github_models_token` secret to use it  (provider_misconfigured)
Tip: Store the API key with `agentctl secret set <name> <value>`
```

If the failure came from inside your Lua code, a cleaned-up stack trace points at the line that raised it:

```
Error: Something exploded  (lua_error)

Stack trace:
  helpers.lua:313  in structured
  init.lua:53
```

The code in parentheses identifies the kind of error — the full list is in the [protocol reference](/v0/reference/protocol#error-codes). With `call --compact`, errors come back as one-line JSON instead, for scripts that want to parse them.

## See also

- [WebSocket protocol](/v0/reference/protocol) — for building your own client
- [Configuration reference](/v0/reference/configuration) — daemon flags and `config.toml`
- [Permissions & grants](/v0/security/grants) — how access control works
- [Observability](/v0/operations/observability) — traces and logging

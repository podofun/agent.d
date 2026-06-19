# Quick start

Get agent.d running locally in about five minutes. You'll start the daemon with the bundled example, verify it's alive, list loaded tools, call an action, and follow the live trace.

## Prerequisites

You've already installed the binaries. If not, see [Installation](/v0/guide/installation) first.

::: tip Using a release download?
These steps use the bundled `examples/` files, which ship in the source
repository. If you installed pre-built binaries, clone the repo to get the
examples (`git clone https://github.com/podofun/agent.d`) and run the commands
from there — or skip straight to the [Tutorial](/v0/tutorial/), which builds a
project from scratch with no checkout required.
:::

## Step 1 — Start the daemon

Run the daemon with the bundled example entry file and grants:

::: code-group

```bash [release]
daemon \
  --init examples/init.lua \
  --grants-file examples/grants.toml
```

```bash [cargo]
cargo run -p daemon -- \
  --init examples/init.lua \
  --grants-file examples/grants.toml
```

:::

The daemon evaluates `examples/init.lua`, which pulls in:

- `examples/tools/git.lua` — registers the `git` tool with `git.status`, `git.diff`, and related actions.
- `examples/skills/` — loads every `.md` skill file in that directory.
- An inline `terse` skill.
- `examples/runners/backend_reviewer.lua` — registers a runner wired to the git tool.

`examples/grants.toml` grants the `git` tool permission to run the `git` binary (`shell.exec:git`).

The startup banner prints the bind address and component counts. By default the daemon listens on `127.0.0.1:7777`.

## Step 2 — Check liveness

Open a second terminal and confirm the daemon is up:

::: code-group

```bash [release]
agentctl health
```

```bash [cargo]
cargo run -p agentd-cli -- health
```

:::

You should see `ok`.

## Step 3 — List loaded tools

::: code-group

```bash [release]
agentctl tools
```

```bash [cargo]
cargo run -p agentd-cli -- tools
```

:::

This prints every registered action name, for example `git.status`, `git.diff`, and others registered by the example.

## Step 4 — Call an action

::: code-group

```bash [release]
agentctl call git.status
```

```bash [cargo]
cargo run -p agentd-cli -- call git.status
```

:::

The response is `{ "result": …, "duration_ms": … }`. Add `--result-only` to print only the action's return value, or `--compact` to suppress pretty-printing.

You can pass arguments with `-d`:

```bash
agentctl call git.status -d cwd=/path/to/repo
```

## Step 5 — Follow the trace

In a third terminal, stream the live trace:

::: code-group

```bash [release]
agentctl trace -f
```

```bash [cargo]
cargo run -p agentd-cli -- trace -f
```

:::

Each call you make in step 4 appears as a JSONL event. The trace is also written to `$XDG_STATE_HOME/agentd/trace.jsonl`. Use `-n <N>` to show the last N lines instead of following.

## What just happened

1. The daemon loaded your Lua components once and started serving on `127.0.0.1:7777`.
2. `agentctl` connected to `/ws` with the auto-minted bearer token and sent a `tools.list` or `actions.call` request.
3. The permission engine checked `shell.exec:git` against `grants.toml` and allowed the call.
4. The `git.status` handler received a `ctx` handle, ran the `git` binary, and returned the result.
5. The result was sent back to `agentctl` and appended to the trace log.

::: tip Connect from anywhere
Set `AGENTD_URL` or pass `--url` to point `agentctl` at a daemon running on a different address or port.
:::

## Next steps

Work through the tutorial to build your own tools, configure permissions, and wire up a runner:

- [Tutorial](/v0/tutorial/)

## See also

- [How it works](/v0/guide/how-it-works)
- [Writing tools](/v0/writing/tools)
- [Security: grants](/v0/security/grants)
- [Reference: CLI](/v0/reference/cli)

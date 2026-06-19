# Step 5 — Calling the Agent

With all the pieces in place you can start the daemon and exercise every component: health check, tool listing, raw action calls, and a full runner invocation.

## Start the daemon

Point the daemon at your project's `init.lua` and `grants.toml`:

::: code-group
```bash [release]
daemon \
  --init ~/projects/git-reviewer/init.lua \
  --grants-file ~/projects/git-reviewer/grants.toml
```
```bash [cargo]
cargo run -p daemon -- \
  --init ~/projects/git-reviewer/init.lua \
  --grants-file ~/projects/git-reviewer/grants.toml
```
:::

The startup banner confirms what was loaded, for example:

```
Local  http://127.0.0.1:7777
WS     ws://127.0.0.1:7777/ws
Control ws://127.0.0.1:7777/control
actions=2  runners=1  services=0  skills=1
```

Leave this running and open a second terminal for `agentctl`.

## Check health

```bash
agentctl health
```

Returns `ok` when the daemon is reachable. This is the only endpoint that does not require authentication.

## List registered tools

```bash
agentctl tools
```

You should see `git.diff` and `git.status` in the output.

## Call an action directly

`agentctl call` invokes a single action and prints the result:

```bash
agentctl call git.status
```

Output includes the result and the duration:

```json
{
  "result": { "status": " M tools/git.lua\n", "exit_code": 0 },
  "duration_ms": 12
}
```

### Pass arguments

Use `-d key=value` pairs for simple values. Values are parsed as JSON first, falling back to a string:

```bash
agentctl call git.diff -d staged=true
agentctl call git.diff -d cwd=/path/to/repo -d staged=true
```

For complex argument shapes, pass the whole object as `--json`:

```bash
agentctl call git.diff --json '{"staged": true, "cwd": "/path/to/repo"}'
```

::: info Mutual exclusion
`--json` and `-d` are mutually exclusive. Use one or the other.
:::

### Trim the output

`--result-only` drops the `duration_ms` wrapper and prints only the `result` value:

```bash
agentctl call git.status --result-only
```

`--compact` prints the JSON without indentation:

```bash
agentctl call git.status --compact
```

## Run the runner

`agentctl runner run` sends a natural-language prompt to the runner and streams back the model's response:

```bash
agentctl runner run backend_reviewer "review my staged diff"
```

The runner calls `git.diff` (with `staged = true` if it decides to) and `git.status`, reads the output, and returns a review. The result includes the model and stop reason:

```json
{
  "text": "…review text…",
  "provider": "anthropic",
  "model": "claude-opus-4-7",
  "stop_reason": "end_turn"
}
```

`--text-only` strips the envelope and prints only the `text` field — useful when piping to another command:

```bash
agentctl runner run backend_reviewer "review my staged diff" --text-only
```

## Inspect registered components

```bash
agentctl runner ls
agentctl runner inspect backend_reviewer
agentctl skills ls
agentctl skills inspect reviewer
```

## Next step

[Step 6 — The dev loop →](/v0/tutorial/dev-loop)

## See also

- [CLI reference](/v0/reference/cli)
- [WebSocket protocol](/v0/reference/protocol)
- [Concepts: interfaces and callers](/v0/concepts/interfaces-and-callers)
- [Security: grants](/v0/security/grants)

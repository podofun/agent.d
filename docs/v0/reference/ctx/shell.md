# ctx.shell: Shell Execution

`ctx.shell` runs a child process from an explicit argument vector. No shell parses the command: each entry in `args` reaches the program as one argument, so input that contains quotes, spaces or `;` cannot inject extra commands.

**Required permission:** `shell.exec` or `shell.exec:<bin>`. See [Security: permission slugs](/v0/security/permission-slugs) and [Security: sandbox](/v0/security/sandbox).

## Signatures

```lua
-- Short form
ctx.shell(bin: string, args?: string[], opts?: { cwd?: string, stdin?: string, separate_stderr?: boolean, timeout_ms?: integer })
  -> { stdout: string, stderr: string, exit_code: integer?, timed_out: boolean }

-- Structured form
ctx.shell({ bin: string, args?: string[], cwd?: string, stdin?: string, separate_stderr?: boolean, timeout_ms?: integer })
  -> { stdout: string, stderr: string, exit_code: integer?, timed_out: boolean }
```

Both forms are identical in behavior; use whichever is cleaner at the call site.

## Parameters

| Parameter | Type | Default | Description |
|---|---|---|---|
| `bin` | `string` | required | The binary name or absolute path to execute. |
| `args` | `string[]` | `{}` | Argument vector (not a shell string). |
| `cwd` | `string` | inherited | Working directory for the child process. Without it, the program starts in the action's working directory: the chat's folder, the runner's `cwd`, or the configuration folder. |
| `stdin` | `string` | none | Data piped to the child's standard input. |
| `separate_stderr` | `boolean` | `true` | When `true`, stderr is captured separately in `result.stderr`. When `false`, stderr is merged into `result.stdout`. |
| `timeout_ms` | `integer` | none | Stop the program after this many milliseconds. Must be a whole number of at least 1. |

## Return value

| Field | Type | Description |
|---|---|---|
| `stdout` | `string` | Captured standard output. |
| `stderr` | `string` | Captured standard error (empty when `separate_stderr` is `false`). |
| `exit_code` | `integer` or `nil` | Process exit code. Zero means success. `nil` when the program was stopped by `timeout_ms`. |
| `timed_out` | `boolean` | `true` when `timeout_ms` ran out and the program was stopped. |

A non-zero exit does not raise an error, so check `result.exit_code` yourself.

## Stopping a program

With `timeout_ms`, agent.d stops the program when the time runs out, together with every program it started, and returns what it printed until then with `timed_out = true`. The same happens when the run that called `ctx.shell` is cancelled. Without `timeout_ms`, a program can run as long as it likes.

```lua
local result = ctx.shell("cargo", { "test" }, { timeout_ms = 120000 })
if result.timed_out then
  return { error = "the tests did not finish in two minutes", output = result.stdout }
end
```

A program that starts its own session, for example with `setsid`, leaves the group agent.d stops, and keeps running.

## Permission

The permission slug is `shell.exec:<bin>`, where `<bin>` is the first argument, for example `shell.exec:git` for `ctx.shell("git", …)`. A bare `shell.exec` grant allows every program.

Grant example in `grants.toml`:

```toml
[tool.git]
granted = ["shell.exec:git"]
```

### When the program is not granted

If the tool does not have the grant, agent.d asks before the program runs. The approval request shows the full command (`argv`) and its working directory (`cwd`), so the approver sees what will run. An allow covers this one run of the command. If the handler runs the same command again, agent.d asks again, because the command can do something different after the agent changes files. See [Interactive approvals](/v0/security/approvals).

A call that runs outside a handler, for example while `init.lua` loads, cannot wait for an answer. It is refused.

### When a denial applies

agent.d checks `[denial]` before it asks anything:

- A denied program is refused, whether the call names it by name or by path. See [Programs](/v0/security/denials#programs).
- A command is refused when its sandbox could reach a denied file or host. See [Shell commands](/v0/security/denials#shell-commands).

## Examples

```lua
-- Short form: run git and check the result
agentd.action("git.status", function(args, ctx)
  local r = ctx.shell("git", { "status", "--short" }, { cwd = args.path })
  if r.exit_code ~= 0 then
    error("git status failed:\n" .. r.stderr)
  end
  return r.stdout
end)
```

```lua
-- Structured form: pipe data to stdin
agentd.action("fmt.json", function(args, ctx)
  local r = ctx.shell({
    bin  = "jq",
    args = { "." },
    stdin = args.input,
  })
  if r.exit_code ~= 0 then
    error(r.stderr)
  end
  return r.stdout
end)
```

::: warning Argv-only
Put the program name in `bin` and each argument in `args`. A whole command such as `"git status --short"` passed as `bin` is treated as the name of a single program.
:::

::: tip Sandbox
In production the shell executor runs inside the native sandbox. See [Security: sandbox](/v0/security/sandbox) for filesystem and network confinement details.
:::

## See also

- [Security: sandbox](/v0/security/sandbox)
- [Security: permission slugs](/v0/security/permission-slugs)
- [Security: grants](/v0/security/grants)
- [ctx overview](/v0/reference/ctx/)

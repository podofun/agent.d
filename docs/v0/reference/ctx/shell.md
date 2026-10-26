# ctx.shell — Shell Execution

`ctx.shell` runs a child process using an explicit argument vector. There is no shell string interpretation — each token is a discrete argument, which prevents injection attacks by construction.

**Required permission:** `shell.exec` or `shell.exec:<bin>` — see [Security: permission slugs](/v0/security/permission-slugs) and [Security: sandbox](/v0/security/sandbox).

## Signatures

```lua
-- Short form
ctx.shell(bin: string, args?: string[], opts?: { cwd?: string, stdin?: string, separate_stderr?: boolean })
  -> { stdout: string, stderr: string, exit_code: integer }

-- Structured form
ctx.shell({ bin: string, args?: string[], cwd?: string, stdin?: string, separate_stderr?: boolean })
  -> { stdout: string, stderr: string, exit_code: integer }
```

Both forms are identical in behavior; use whichever is cleaner at the call site.

## Parameters

| Parameter | Type | Default | Description |
|---|---|---|---|
| `bin` | `string` | required | The binary name or absolute path to execute. |
| `args` | `string[]` | `{}` | Argument vector (not a shell string). |
| `cwd` | `string` | inherited | Working directory for the child process. |
| `stdin` | `string` | none | Data piped to the child's standard input. |
| `separate_stderr` | `boolean` | `true` | When `true`, stderr is captured separately in `result.stderr`. When `false`, stderr is merged into `result.stdout`. |

## Return value

| Field | Type | Description |
|---|---|---|
| `stdout` | `string` | Captured standard output. |
| `stderr` | `string` | Captured standard error (empty when `separate_stderr` is `false`). |
| `exit_code` | `integer` | Process exit code. Zero means success. |

`ctx.shell` does **not** raise an error on non-zero exit — check `result.exit_code` yourself.

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
Never pass a shell string like `"git status --short"` as `bin`. The entire string would be treated as the binary name. Always split your command into `bin` + `args`.
:::

::: tip Sandbox
In production the shell executor runs inside the native sandbox. See [Security: sandbox](/v0/security/sandbox) for filesystem and network confinement details.
:::

## See also

- [Security: sandbox](/v0/security/sandbox)
- [Security: permission slugs](/v0/security/permission-slugs)
- [Security: grants](/v0/security/grants)
- [ctx — overview](/v0/reference/ctx/)

# agentd-shell

Process exec primitive.

`ExecRequest { bin, args, cwd, stdin, separate_stderr }` → `ExecResult`.

**Argv only — no shell interpreter.** Permission gating lives in the caller (the
scripting `ctx.shell` binding), not here.

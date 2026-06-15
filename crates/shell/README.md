# agentd-shell

Process exec primitive.

`ExecRequest { bin, args, cwd, stdin, separate_stderr, sandbox }` → `ExecResult`.

**Argv only — no shell interpreter.** Permission gating lives in the caller (the
scripting `ctx.shell` binding), not here.

## Native shell sandbox

`sandbox: Option<SandboxPolicy>` confines the spawned child to a set of
readable/writable filesystem subtrees plus a coarse network on/off, enforced by
the OS:

| Platform | Backend | Applied via |
| --- | --- | --- |
| Linux | Landlock | `pre_exec` self-restriction in the forked child |
| macOS | Seatbelt | argv wrapped in `sandbox-exec -p <SBPL>` |
| Windows | AppContainer | spawn under an AppContainer SID (Phase 1: not yet enforcing → `is_supported()` is false, so `ctx.shell` fails closed) |

`exec` fails closed (`ShellError::SandboxUnavailable`) when a policy is requested
but no backend can enforce it. `SandboxPolicy.unrestricted` (set by the
`shell.unrestricted` grant) skips the sandbox. The grant→policy translation
lives in the caller (`agentd-scripting`); this crate only enforces the policy it
is handed.

Network enforcement is coarse on/off in Phase 1. Host-granular `net:<host>` on
children is Phase 2 (network namespace + SNI proxy).

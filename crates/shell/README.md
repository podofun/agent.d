# agentd-shell

Process exec primitive.

`ExecRequest { bin, args, cwd, stdin, separate_stderr, sandbox }` → `ExecResult`.

**Argv only — no shell interpreter.** Permission gating lives in the caller (the
scripting `ctx.shell` binding), not here.

## Native shell sandbox

`sandbox: Option<SandboxPolicy>` confines the spawned child's filesystem
(read/write subtrees) and network (host-granular `net:<host>`), enforced by the
OS.

### Filesystem

| Platform | Backend | Applied via |
| --- | --- | --- |
| Linux | Landlock | `pre_exec` self-restriction in the forked child |
| macOS | Seatbelt | argv wrapped in `sandbox-exec -p <SBPL>` |
| Windows | restricted token + capability SIDs | `CreateRestrictedToken` + ACL grants (planned; `is_supported()` false until landed → fails closed) |

### Network (host-granular)

When `allow_net` is set, the child is confined so an in-process egress proxy
(`src/proxy/`) is its ONLY route out. The proxy reads the destination host from
the TLS SNI / HTTP `Host` / `CONNECT` target (no TLS termination, no MITM) and
admits it only if a `net:<host>` slug in `net_hosts` `Permission::covers` it.

| Platform | Containment |
| --- | --- |
| Linux | rootless netns (`CLONE_NEWUSER\|CLONE_NEWNET`), sole egress = proxy via an `execve`'d in-netns supervisor + anonymous control socketpair + SCM_RIGHTS fd passing |
| macOS | Seatbelt `(deny network*)` + allow only the proxy's exact loopback port |
| Windows | sandbox user + firewall/WFP allowing only the proxy port (planned) |

`exec` fails closed (`ShellError::SandboxUnavailable`) when a policy is requested
but no backend can enforce it (including when unprivileged user namespaces are
disabled on Linux). `SandboxPolicy.unrestricted` (the `shell.unrestricted` grant)
skips the sandbox. The grant→policy translation lives in the caller
(`agentd-scripting`); this crate only enforces the policy it is handed.

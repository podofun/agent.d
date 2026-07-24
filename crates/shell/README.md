# agentd-shell

`agentd-shell` executes argv-based child processes with an explicit sandbox policy.

`ExecRequest` controls the program, arguments, working directory, standard input, standard-error handling, and sandbox. `SandboxPolicy` carries approved filesystem and network scopes. `ExecResult` contains the exit code, standard output, and standard error.

Platform enforcement:

- Linux uses Landlock for filesystem access and network namespaces, seccomp, and packet filtering for network access.
- macOS uses sandbox profiles and an optional privileged broker for transparent network filtering.
- Windows uses AppContainer isolation, access-control entries, and Windows Filtering Platform rules.

Unsupported sandbox configurations fail closed. Callers must derive the policy from effective grants.

# agentd-process

`agentd-process` provides an argv-based wrapper around `tokio::process::Command`.

The wrapper configures arguments, environment variables, standard input, and output capture. It exposes process spawning and output collection without accepting a shell command string.

On Windows, it resolves PowerShell, batch, and command-file shims before it starts the process. Use `command(program)` to create a command.

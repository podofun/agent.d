# What a denial guarantees

A denial in `[denial]` blocks something for everyone, and a call it blocks is refused without a question. This page explains where agent.d checks denials, how each kind is matched, and what a denial does not cover. Read it before you rely on a denial to protect a secret file, a host, or a program.

```toml
[denial]
actions     = ["pkg.publish"]
permissions = ["fs.read:/home/me/project/.env", "net:tracker.example.com", "shell.exec:curl"]
```

## Where agent.d checks a denial

agent.d checks denials in each of these places:

- **Action calls.** A denied action, or an action that needs a denied permission, is refused. This applies to clients, runners, services, and `ctx.call`.
- **Calls inside a handler.** `ctx.fs`, `ctx.http`, `ctx.ws`, `ctx.mailer`, `ctx.secret`, `ctx.shell`, and the other `ctx` calls check the denials before they check the grants.
- **Approvals.** A denied call is refused before any question is asked, so there is nothing to approve, and "allow forever" cannot add it.
- **Programs that `ctx.shell` starts.** See [Shell commands](#shell-commands).
- **AI providers that run their own tools.** See [AI providers](#ai-providers).

agent.d reads the denials at the moment of each call. When you change `[denial]` and the daemon reloads, the next call uses the new denials.

## Files

A file denial, such as `fs.read:/home/me/project/.env`, compares paths the same way as a file grant:

- agent.d follows symlinks and removes `..` segments before it compares. A link to a denied file is the denied file.
- On macOS and Windows, uppercase and lowercase letters are the same. `/Home/Me/Project/.ENV` is the denied file too.
- agent.d opens the file and then confirms that the open file is the file it checked. If the path changes between the check and the read, for example because a link was replaced, the read is refused.

A file denial must be an absolute path. A relative path or a `~` path is an error when agent.d loads `grants.toml`.

## Hosts

A host denial, such as `net:tracker.example.com`, applies to `ctx.http`, `ctx.ws`, and `ctx.mailer`:

- agent.d compares host names in one standard form. Uppercase letters, a dot at the end, and international names in either form all match the same denial.
- agent.d compares IP addresses in one standard form too. For example, an IPv4 address written as an IPv6 address matches the IPv4 denial.
- agent.d checks every redirect of `ctx.http`. A redirect to a host that is denied or not granted stops the request. agent.d follows at most 10 redirects.
- agent.d checks the address that it actually connects to. If a granted host name points to a denied IP address, the connection is refused.
- `ctx.ws` and `ctx.mailer` check the denials again each time they send. A connection that opened before a new denial cannot send after it.
- `ctx.http` ignores proxy settings in the environment, so a proxy cannot hide the real destination.

## Programs

A program denial, such as `shell.exec:curl`, stops that program for `ctx.shell`. agent.d finds the program file that the denial names and the program file that the call names, and compares the two files:

- A name is found on the daemon's `PATH`. A path is used as it is, from the call's working directory.
- Symlinks are followed, so `shell.exec:python3` also stops `/usr/bin/python3` and a link to it.
- On Windows, a name without `.exe` also matches the `.exe` file.

A bare `shell.exec` denial stops every program.

## Shell commands

A program that `ctx.shell` starts runs in the [shell sandbox](/v0/security/sandbox). The sandbox opens the folders and hosts that the call is granted. It works by opening access, and it cannot cut one file out of a folder it opens, because a program inside can rename folders and create links.

For this reason, when a shell command could reach something you denied, agent.d refuses the command before it starts, without a question. The error says what was denied and how the command could reach it.

For files, a command can reach a denied path when:

- A folder that the command can read or write contains the denied path, or the denied path contains that folder. The folders include the command's grants and the system folders that every program needs, such as `/usr` and `/etc`. On Linux and macOS, they also include your `~/.config` folder.
- On Linux, a mount shows the denied folder under another name inside a folder that the command can reach.
- The denied file has more than one hard link, and the command can reach a folder on the same disk.
- On Windows, the file's access list lets sandboxed programs read or write it. This can be access that an earlier command left, or a hard link through a granted folder. agent.d also checks the folder of the program, which it opens when Windows cannot load the program from where it is. When the denials change on reload, agent.d removes the access that earlier commands left.

For hosts, a command can reach a denied host when:

- One of its `net:` grants covers the denied host, or the denied host covers the grant. A bare `net` grant covers every host.
- An IP address is denied, and the command has any host-name grant. A host name can point to any address.
- On Windows, the command has network access, and you denied a local address or one of the machine's name servers. Windows lets every networked command reach these.

A `net:` grant that a denial fully covers is removed before the command starts, and the command runs without it.

A command with the `shell.unrestricted` grant runs without a sandbox, so it is refused while any file or host is denied.

### Fix a refused shell command

A refusal looks like this:

```text
this shell command could read what `fs.read:/home/me/project/.env` denies in grants.toml,
because its sandbox opens `/home/me/project`, which overlaps it. A sandbox cannot carve a
denied path out of a folder it opens, so the command is refused before anything is asked.
Keep denied files outside the folders shell commands are granted, or grant narrower folders
```

To fix it, do one of these:

- Move the denied file out of the folders that you grant to shell commands. For example, keep secrets outside the project folder.
- Grant narrower folders. For example, grant `fs.read:/home/me/project/src/**` and leave the rest of the project out.
- Grant exact hosts. Avoid wide patterns such as `net:*`.

## AI providers

Some AI providers run their own tools: they read files and run commands outside agent.d. agent.d cannot check those calls against your denials, so it refuses to use these providers while you deny any file or host:

- `codex` and `openai-cli` run Codex, which has its own tools. A run or a `ctx.ai` call on these providers is refused while `[denial]` has any `fs.read`, `fs.write`, or `net` entry. The error names the denial and suggests a provider that agent.d controls, such as `anthropic` or `openai`.
- `anthropic-cli` runs Claude Code with all of its own tools turned off. Its only tools are the agent.d actions of the runner, so agent.d checks every call.

Other denials, such as `secret:` or a denied action, do not stop these providers.

## Limits

A denial does not cover the following cases, so plan around them:

- **A running command keeps its access.** A new denial applies to calls that start after the reload. A shell command or an AI run that is already busy keeps the access that it started with.
- **A shell command can see that a denied file exists.** It cannot read the contents, but it can see the name and the size.
- **A host-name denial stops connections.** A command can still look up the name. A server that is reachable by another name, or directly by its address, stays reachable. Deny the addresses too if that matters.
- **A program denial stops only that program.** A granted shell or build tool, such as `sh` or `make`, can still start the denied program. Keep interpreters away from tools that must not run a denied program.

## See also

- [grants.toml reference](/v0/security/grants)
- [Shell sandbox](/v0/security/sandbox)
- [Interactive approvals](/v0/security/approvals)
- [Permission slugs](/v0/security/permission-slugs)
- [Best practices](/v0/security/best-practices)

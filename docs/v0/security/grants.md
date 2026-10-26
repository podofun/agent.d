# grants.toml reference

`grants.toml` is where you decide what your agents may do. Each component says what it needs, but no component can give itself access. Only this file gives access. It also holds the rules that nobody can override, and it says who answers when an agent asks for more.

Read this page when you add a tool, a runner, or a service, or when a call fails with a permission error.

## Location and loading

The default path is `$XDG_CONFIG_HOME/agentd/grants.toml`. To use a different file, pass `--grants <path>` or set `AGENTD_GRANTS`. The old `--grants-file` and `AGENTD_GRANTS_FILE` names still work, but they are deprecated.

`init.lua` and `grants.toml` live in the same folder. That folder is the **workspace root**. If you give agent.d one of the two files, it finds the other one next to it. Use both flags only when the files are in different folders.

When you run the daemon with `--watch`, it loads the file again each time you save it.

### The file is strict

agent.d reads every line of the file and refuses to start if a line is wrong. A rule that does nothing is dangerous, because you think it protects you. So these mistakes stop the load and tell you which line to fix:

- An unknown section or key, for example a misspelled `[denail]` or `alowed_actions`.
- A pattern that can never match. A `*` is supported only at the end of a value, as in `api.*` or `/srv/app/**`. So `net:*.example.com` is an error.
- An IPv6 address prefix such as `net:2001:db8:*`. agent.d compares IPv6 addresses in their shortest form, so a prefix can fail to match. Grant or deny full addresses.
- A file denial that is not an absolute path, such as `fs.read:.env` or `fs.read:~/.ssh/**`.
- An `approval` key outside a `[runner.<name>]` section.
- A `[policy]` section. `[denial]` and `[approval]` replaced it, and the error tells you where each key moved.

With `--watch`, a file with an error does not replace the running setup. The daemon prints the error and keeps your last good configuration until you save a fixed file.

## How a call is decided

agent.d checks every action call in two stages.

**First, the denials.** These are rules that nobody can override. If one of them matches, the call is refused immediately, and nobody is asked:

1. The action is in `[denial].actions`.
2. A permission that the action needs is in `[denial].permissions`.
3. The call comes from a service, and the action is not in that service's `allowed_actions` list.
4. The call comes from an interface, and the action is not in that interface's `allowed_actions` list.

**Then, the questions.** These are things that are not granted yet. agent.d asks about them one at a time, in this order:

1. The call comes from a runner, and the action is not in that runner's `allowed_actions` list.
2. The tool does not have every permission that the action needs.
3. The action has `confirm = true`, and `[approval].skip` does not list it.

If all checks pass, the call runs. If a question is not answered with an allow, the call is refused. Who answers the questions depends on the run's approval setting. See [Interactive approvals](/v0/security/approvals).

An approval never skips a later check. After each answer, agent.d checks the call again from the start.

## Schema

### `[tool.<name>]`

Gives permissions to every action of a tool. The name is the `name` that you pass to `agentd.tool{...}` in Lua.

```toml
[tool.git]
granted = ["shell.exec:git"]
```

### `[runner.<name>]`

Controls a runner. A runner is an AI agent: a model, a system prompt, and the actions it can use.

- `granted` must include `ai:<provider>` for the provider of the runner's model.
- `allowed_actions` lists the actions the runner may call without a question. If an action is not on the list, agent.d asks before the runner can call it.
- `approval` sets who answers this runner's questions by default: `"human"`, `"classifier"`, or `"nobody"`. If you do not set it, the value is `"human"`. See [who answers](/v0/security/approvals#who-answers).

```toml
[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]
approval = "human"
```

The approval setting is in `grants.toml` and never in Lua. This way, a runner cannot choose its own setting.

### `[interface.<name>]`

Limits which actions a connected client may call. An interface is a client that connects with its own token, such as a web backend or a chat bridge. If the list is empty, or the section does not exist, this check does not limit the client.

```toml
[interface.telegram]
allowed_actions = ["git.status"]
```

### `[service.<name>]`

Gives permissions to a service. A service is a long-running background task, such as a chat gateway. `allowed_actions` is optional. If you set it, the service can call only those actions.

```toml
[service.discord_handler]
granted = [
  "ai:openai",
  "net:discord.com",
  "memory.read:discord/**",
  "memory.write:discord/**",
]
allowed_actions = ["discord.send"]
```

A service that calls actions must have a section here, even an empty one.

### `[package.<name>]`

Approves all the permissions that an installed package declares. Without this entry, the package gets no permissions.

```toml
[package.acme]
trusted = true
```

With `trusted = true`, every component of the package gets the permissions from its `package.toml`. To narrow one component, write its own entry, such as `[tool."acme/git"]`. An entry for a component always has priority over the package grant.

### `[denial]`

Lists what nobody may do or reach. A denial applies to everyone, including you, and agent.d never asks about it.

```toml
[denial]
actions     = ["pkg.publish"]
permissions = ["net:tracker.example.com", "fs.read:/home/me/project/.env"]
```

Use a denial for something that must never happen, whatever the grants say. A denial applies everywhere that agent.d checks access: in action calls, inside running handlers, in programs that `ctx.shell` starts, and in the AI providers. A file denial must be an absolute path. See [What a denial guarantees](/v0/security/denials) for the full list and its limits.

### `[approval]`

Controls how questions are answered.

```toml
[approval]
classifier = "approval_classifier"
skip       = ["deploy.preview"]
```

- `classifier` names the runner that answers questions when a run's setting is `"classifier"`. See [The classifier](/v0/security/approvals#the-classifier).
- `skip` lists actions with `confirm = true` that are never asked about. When you choose "allow forever" for a confirm question, agent.d adds the action here for you. You can also edit the list by hand.

## Full annotated example

```toml
# Permissions for each tool
[tool.git]
granted = ["shell.exec:git"]

# A runner, the actions it may call, and who answers its questions
[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]
approval = "human"

# A runner that answers approval questions. It has no actions.
[runner.approval_classifier]
granted = ["ai:anthropic"]

# Limit what one client may call
[interface.telegram]
allowed_actions = ["git.status"]

# A background service
[service.discord_handler]
granted = [
  "ai:openai",
  "net:discord.com",
  "memory.read:discord/**",
  "memory.write:discord/**",
]
allowed_actions = ["discord.send"]

# Approve a package
[package.acme]
trusted = true

# What nobody may do
[denial]
actions     = ["pkg.publish"]
permissions = ["fs.read:/home/me/project/.env"]

# How questions are answered
[approval]
classifier = "approval_classifier"
skip       = []
```

## Key rules to remember

- A component's `requires` field says what it needs. It never gives access.
- A component's own entry always has priority over a package grant.
- For interfaces and services, an empty `allowed_actions` list means "no limit". For runners, an action that is not on the list causes a question.
- agent.d resolves file paths before it compares them. It follows symlinks and removes `..` segments, so an alias cannot get around a grant or a denial. On macOS and Windows, uppercase and lowercase letters in paths are the same.
- A relative `fs.read` or `fs.write` grant starts from the component's working directory, which is the workspace root by default. So `fs.read:notes/**` works on every machine. An absolute grant stays as you wrote it.
- A `shell.exec` value is a program name or a path. agent.d finds the program file that the value names, so `shell.exec:python3` and `/usr/bin/python3` are the same program.

## See also

- [Permission slugs](/v0/security/permission-slugs)
- [Interactive approvals](/v0/security/approvals)
- [What a denial guarantees](/v0/security/denials)
- [Permissions concept](/v0/concepts/permissions)
- [Best practices](/v0/security/best-practices)

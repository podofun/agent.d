# Permissions

agent.d is default-deny. An action can touch your files, the network, your secrets or an AI provider only after you allow it. This page explains how agent.d makes each decision, so you can predict the outcome of a call and find the cause when one fails.

## Two stages: denials, then questions

agent.d checks each action call in two stages.

**The denials come first.** A denial is a rule that nobody can override, and a call it blocks is refused without a question. A call is refused at once when:

- The action is in `[denial].actions`.
- A permission that the action needs is in `[denial].permissions`.
- The call comes from a service or an interface, and the action is not in its `allowed_actions` list.

**The questions come next.** Each question is about something you have not granted yet. agent.d asks them one at a time, in this order:

1. The call comes from a runner, and the action is not in the runner's `allowed_actions` list.
2. The tool does not have every permission that the action needs.
3. The action has `confirm = true`, and `[approval].skip` does not list it.

When all checks pass, the call runs. When a question gets an allow, agent.d checks the call again from the start, and the remaining checks still apply. The run's approval setting decides who answers: a human, a classifier, or nobody. See [Interactive approvals](/v0/security/approvals).

| Check | What it asks | Where you configure it |
|---|---|---|
| Denied action | Is the action denied for everyone? | `[denial].actions` |
| Denied permission | Does the action need a denied permission? | `[denial].permissions` |
| Service allowlist | Can this service call the action? | `[service.<name>].allowed_actions` |
| Interface allowlist | Can this client call the action? | `[interface.<name>].allowed_actions` |
| Runner allowlist | Can this runner call the action without a question? | `[runner.<name>].allowed_actions` |
| Tool grants | Does the tool have what the action needs? | `[tool.<name>].granted`, `[package.<name>]` |
| Confirm | Does someone approve this call? | `confirm = true` in Lua, `[approval].skip` |

For interfaces and services, an empty `allowed_actions` list means "no limit". For runners, an action that is not on the list causes a question.

## grants.toml is the only source of grants

A component says what it needs in its `requires` field. Access itself comes from `grants.toml`:

```toml
# The git tool can run the git program
[tool.git]
granted = ["shell.exec:git"]

# The reviewer runner can call these two actions without a question
[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]

# A Telegram client can call only git.status
[interface.telegram]
allowed_actions = ["git.status"]

# A service has its own grants and an optional action list
[service.discord_handler]
granted         = ["ai:openai", "net:discord.com", "memory.read:discord/**", "memory.write:discord/**"]
allowed_actions = ["discord.send"]

# Approve all the permissions that a package declares
[package.acme]
trusted = true

# What nobody may do
[denial]
actions     = ["pkg.publish"]
permissions = []
```

See the [grants.toml reference](/v0/security/grants) for every section and key.

## Permission slugs

A permission is a slug with the form `domain[:specifier]`. A `*` at the end of the specifier matches more values:

| Slug | What it covers |
|---|---|
| `shell.exec` | Run any program with `ctx.shell`. |
| `shell.exec:git` | Run the `git` program only. |
| `net:<host>` | HTTP or WebSocket calls to that host. |
| `net:*` | HTTP or WebSocket calls to any host. |
| `fs.read:<glob>` | Read the files that match the glob. |
| `fs.write:<glob>` | Write the files that match the glob. |
| `secret:<key>` | Read or write one secret. |
| `secret:*` | Read or write any secret. |
| `memory.read:<ns-glob>` | Read memory namespaces that match the glob. |
| `memory.write:<ns-glob>` | Write memory namespaces that match the glob. |
| `ai:<provider>` | Model calls through one provider. |
| `ai:*` | Model calls through any provider. |
| `oauth:<provider>` | An OAuth grant for a provider. |

File globs follow paths: `fs.write:/tmp/**` covers every file under `/tmp/`. See [Permission slugs](/v0/security/permission-slugs) for the full rules.

## File paths are resolved first

Before agent.d compares a file path with a grant or a denial, it resolves the path: it follows symlinks and removes `..` segments. A rule applies to the real file, whatever name reaches it. For example, a grant of `fs.read:/workspace/**` does not cover `/etc/passwd`, even when a symlink inside `/workspace` points there.

## Approvals inside a handler

The checks above happen when an action starts. A handler then uses `ctx` to reach the outside world, and each `ctx` call is checked when it happens. If the handler needs something its tool was not granted, the handler pauses and agent.d asks. An allow covers only that request. The handler's grants, and what the [shell sandbox](/v0/security/sandbox) opens, stay the same.

## Packages declare, grants.toml grants

A package's `package.toml` lists the permissions it needs, so you can see what you are approving. The list grants nothing by itself. To give a package its declared permissions, trust it:

```toml
[package.acme]
trusted = true
```

Without this entry, the package gets no permissions, and its actions cannot use anything that needs a grant.

## `confirm` actions

An action with `confirm = true` needs an approval for each call, unless `[approval].skip` lists it:

```lua
agentd.action({
  name    = "deploy.preview",
  confirm = true,
  handler = function(args, ctx) ... end,
})
```

An approver can allow the call one time, allow it forever, or deny it. "Allow forever" adds the action to `[approval].skip`. If nobody answers within `approval_timeout_ms` (default 120 seconds), the call is refused.

::: warning
"Allow forever" writes to `grants.toml` for you. Read the file after a session to see what you allowed.
:::

## See also

- [grants.toml reference](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)
- [Interactive approvals](/v0/security/approvals)
- [What a denial guarantees](/v0/security/denials)
- [Tutorial: permissions](/v0/tutorial/permissions)

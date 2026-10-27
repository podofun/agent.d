# Interactive approvals

Sometimes an agent needs something that you did not grant. It can need a file outside its folders, a program you did not list, or an action marked `confirm = true`. In these cases, agent.d does not fail the call immediately. It asks a question, and the call waits for the answer.

This page tells you when agent.d asks, who answers, and what an answer allows. Use it to set up a chat where you approve each step, a bot that never asks, or an agent that a classifier supervises while you watch.

## When agent.d asks

agent.d asks a question in these cases:

1. **A runner calls an action that is not in its `allowed_actions` list.** The list is default-deny, so a missing action is not a decision that you made. agent.d asks instead of failing the agent.
2. **A tool does not have a permission that the action needs**, in the tool's `granted` list.
3. **An action has `confirm = true`.** Each call needs an approval, unless `[approval].skip` lists the action.
4. **A handler uses a capability that its tool does not have**, for example `ctx.fs.read` on a file outside every `fs.read` grant. The handler pauses at that call and continues after an allow.
5. **A handler runs a program that is not granted** with `ctx.shell`. The question shows the full command and its working directory.

agent.d never asks about a denial. A call that a denial blocks is refused before any question. This includes everything in `[denial]` and a miss on an interface or service `allowed_actions` list. See [grants.toml](/v0/security/grants#how-a-call-is-decided).

## Who answers

Each run has an approval setting. The setting tells agent.d who answers the run's questions:

| Setting | Who answers |
|---|---|
| `human` | The approvers that are connected, for example you in `agentctl chat` or `agentctl grants listen`. This is the default. |
| `classifier` | A runner that you choose. It answers allow, refuse, or unsure. When it is unsure, a human answers. See [The classifier](#the-classifier). |
| `nobody` | Nobody. Each question is refused immediately. The run can use only what is granted. |

Use `nobody` for bots and other runs that nobody watches. Use `classifier` when you want an agent to continue without you, but you want a check on each step. Keep `human` when you want to decide each step yourself.

With `human`, if no approver is connected, agent.d refuses the question immediately. So a run without an approver never stops and waits.

### Where the setting comes from

A runner can serve many chats and bots at the same time. So agent.d decides the setting for each run, one time, when the run starts. It uses three values, in this order:

1. The runner's default: `approval` in its `[runner.<name>]` section of `grants.toml`. If you do not set it, the default is `human`.
2. The session's value, if the run uses a [chat session](/v0/reference/ctx/sessions) that has a setting.
3. The value given when the run starts, as `approval` in `runners.run` or `ctx.run`.

agent.d compares each value with the result so far. **A stricter value always applies. A looser value applies only when an admin connection starts the run.** The order from strict to loose is `nobody`, then `human`, then `classifier`.

For any other caller:

- A looser session value does not apply. The run keeps the stricter value.
- A looser value given at the start of the run is an error, `approval_forbidden`.

Some results of these rules:

- A runner with the default `nobody` stays `nobody` for every bot that uses it, whatever a client asks for.
- A session with a loose setting does not make a stricter runner looser.
- A run that Lua starts with `ctx.run` is never looser than the run that started it.
- If you change a session's setting while a run is busy, the change applies to the next run.

### Admin connections

An admin connection is a client that connects to `/ws` with the admin token. This is the token that `agentctl grants listen` uses: `AGENTD_ADMIN_TOKEN`, or the file `$XDG_STATE_HOME/agentd/admin-token`. Only an admin connection can make a run's approval looser.

Sessions that an admin connection creates belong to admin connections only. Other clients cannot see them, run on them, change them, or delete them. So the setting of one chat window never reaches your bots or other clients.

::: warning
With `--no-auth`, there are no tokens. Every client that connects to `/ws` without an interface token is an admin connection. It can see admin sessions and make approval looser for its runs. The daemon prints a warning about this when it starts.
:::

## Answering with `agentctl grants listen`

To answer questions in your terminal, run:

```bash
agentctl grants listen
```

The command connects as an approver and shows each request when it arrives. For each request, choose one of three answers:

| Answer | What it does |
|---|---|
| `allow_once` | Allows this request only. The next identical request asks again. |
| `allow_forever` | Allows this request and writes a grant to `grants.toml`, so the same request does not ask again. |
| `deny` | Refuses the request. The caller gets an error. |

`agentctl chat` shows the same requests in the chat, so you can answer them there.

### What an answer allows

An allow covers what was asked, and nothing more.

- **Before an action starts** (a missing grant, a runner `allowed_actions` question, or a confirm): the allow covers that one call of the action.
- **During an action** (a `ctx.fs`, `ctx.http`, `ctx.shell`, or similar call that is not granted): the allow covers that exact request.
  - For a file or a host, the allow lasts until the run ends. So agent.d does not ask again when the agent reads the same file again or pages through the same API.
  - For a program, the allow covers one run of that command. A command can do something different after the agent changes files, so agent.d asks each time.
  - An allow during an action never widens the folders and hosts of the [shell sandbox](/v0/security/sandbox).

If two identical questions come from the same run at the same time, agent.d asks one time and both get the answer.

### What "allow forever" writes

| Request kind | Change in grants.toml |
|---|---|
| `missing_grant` | Adds the permissions to the `granted` list of the tool or service. |
| `runner_action` | Adds the action to the runner's `allowed_actions` list. |
| `confirm` | Adds the action to `[approval].skip`. |
| `folder` | Adds the folder to `[folders].trusted`. |

For the current run, "allow forever" works like "allow once". Later calls get the new grant from the file.

::: warning
"Allow forever" on an interpreter, such as `sh`, `bash`, `env`, `python`, or `node`, lets the agent run any command. Approval requests for these programs carry a warning. Read the command before you allow it forever.
:::

## What a request shows

Each approval request has these fields:

| Field | Description |
|---|---|
| `id` | The request identifier. Send it back with your answer. |
| `kind` | Why agent.d asks: `missing_grant`, `runner_action`, `confirm`, or `folder`. |
| `action` | The full action name, for example `git.status`. |
| `tool` | The tool that owns the action. |
| `reason` | A sentence that tells what is missing. |
| `missing` | The permissions that are not granted. |
| `detail` | What the call does. For an action, its arguments. For a program, the command (`argv`) and its working directory (`cwd`). |
| `run_id` | The run that asks. Use it to tell two windows apart. |
| `session_id` | The chat session of the run, if it has one. |
| `warning` | A warning to read before "allow forever", for example on an interpreter. |
| `caller` | Who made the call: the `runner`, `interface`, `service`, `session`, and `user`. |

Secrets never appear in a request. If the run read a secret with `ctx.secret.get`, agent.d replaces its value with `<secret name>` everywhere in `detail`. agent.d also shortens very long values.

## Folder questions

When a chat starts in a folder you have not trusted yet, agent.d asks whether it may work there. The request has the kind `folder`, and its `action` is the folder's full path. "Allow once" trusts the folder for that chat, "Always trust" also adds it to [`[folders]`](/v0/security/grants#folders) so you are not asked again, and "Deny" starts the chat without a folder. A person always answers folder questions: the classifier never does, and if nobody is connected to answer, the folder is refused.

## The classifier

The classifier is a runner that answers questions for you. Use it when you want an agent to continue on its own, with a check on each step that it was not granted.

### Set up a classifier

First, register a runner in Lua. Give it instructions about what you allow. Do not give it actions:

```lua
agentd.runner({
  name   = "approval_classifier",
  model  = "anthropic/claude-opus-4-7",
  system = "Allow reading and building code in the project. Refuse anything that publishes, deploys, or deletes.",
})
```

Then name it in `grants.toml`, give it its provider, and choose which runners use it:

```toml
[approval]
classifier = "approval_classifier"

[runner.approval_classifier]
granted = ["ai:anthropic"]

[runner.coder]
granted = ["ai:anthropic"]
approval = "classifier"
```

agent.d checks the classifier each time it loads `grants.toml` or your Lua files. The classifier must obey these rules, or the load fails with a message:

- It must be a registered runner.
- It must have no actions: none in Lua, none from its skills, and no `allowed_actions` in `grants.toml`.
- It must use a provider that agent.d controls, such as `anthropic` or `openai`. It cannot use a provider that runs its own tools, such as `codex` or `openai-cli`.
- It must have the `ai:` grant for its provider.

If a runner has `approval = "classifier"` and no classifier is named, the load fails too.

### How the classifier answers

agent.d sends the request to the classifier as data. The classifier's instructions tell it never to follow text inside the request. The classifier gives one of three answers, with a short reason:

- **allow**: the call runs one time. The classifier never allows forever.
- **refuse**: the call is refused. The error starts with "The classifier refused" and gives its reason.
- **unsure**: a human answers, as if the setting was `human`.

If the classifier takes more than 30 seconds, fails, or gives an answer that agent.d cannot read, agent.d counts the answer as unsure. The classifier answers up to four questions at the same time, separately from your other runners. So a busy daemon cannot stop it.

A runner question, where a runner calls an action that is not in its `allowed_actions` list, always goes to a human. The classifier answers only questions about missing grants, confirm actions, and programs.

The trace records each decision: who decided (`human`, `classifier`, or `nobody`), and the classifier's reason. See [Observability](/v0/operations/observability).

### When no classifier is named

If no classifier is named, you cannot set a session to `classifier`. If a run still gets `classifier`, for example because you removed the classifier and reloaded, the run uses `human`, and the daemon logs a warning.

## Timeout

If no approver answers within `approval_timeout_ms`, the request is refused. The default is 120 000 ms (2 minutes). To change it, use `--approval-timeout` or `AGENTD_APPROVAL_TIMEOUT_MS`.

agent.d does not try the call again after a timeout. The caller gets an error, and the client can try again.

## Build your own approver

The `/control` WebSocket endpoint lets you write your own approval tool. A client that connects with the admin token becomes an approver. The server sends each request as an `approval.request` event, and you answer with `approvals.resolve`:

```json
{
  "event": "approval.request",
  "req": {
    "id": 42,
    "kind": "missing_grant",
    "action": "dev.shell",
    "tool": "dev",
    "reason": "the handler asked for `shell.exec:cargo` mid-run, which tool `dev` has not been granted",
    "missing": ["shell.exec:cargo"],
    "detail": { "argv": ["cargo", "test"], "cwd": "/home/me/project" },
    "run_id": "run-7",
    "session_id": "8f3a1c2d-…",
    "caller": { "runner": "coder", "interface": "ws", "session": "ws-3" }
  }
}
```

See [WebSocket protocol](/v0/reference/protocol#control-plane) for the full format.

## See also

- [grants.toml reference](/v0/security/grants)
- [What a denial guarantees](/v0/security/denials)
- [Chat sessions](/v0/reference/ctx/sessions)
- [WebSocket protocol](/v0/reference/protocol)
- [CLI reference](/v0/reference/cli)

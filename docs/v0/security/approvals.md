# Interactive approvals

Sometimes an agent needs something you did not grant: a file outside its folders, a program you did not list, or an action marked `confirm = true`. In these cases agent.d asks a question, and the call waits for the answer.

This page explains when agent.d asks, who answers, and what an answer allows. Use it to set up a chat where you approve each step, a bot that runs on its grants alone, or an agent that a classifier supervises while you watch.

## When agent.d asks

agent.d asks a question in these cases:

1. **A runner calls an action that is not in its `allowed_actions` list.** The list is default-deny, so a missing action is something you have not decided yet. agent.d asks you about it.
2. **A tool lacks a permission that the action needs** in the tool's `granted` list.
3. **An action has `confirm = true`.** Each call needs an approval, unless `[approval].skip` lists the action.
4. **A handler uses a capability its tool does not have**, for example `ctx.fs.read` on a file outside every `fs.read` grant. The handler pauses at that call and continues after an allow.
5. **A handler runs a program that is not granted** with `ctx.shell`. The question shows the full command and its working directory.

Denials do not produce questions. A call that a denial blocks is refused straight away. This covers everything in `[denial]` and a miss on an interface or service `allowed_actions` list. See [grants.toml](/v0/security/grants#how-a-call-is-decided).

## Who answers

Each run has an approval setting that decides who answers its questions:

| Setting | Who answers |
|---|---|
| `human` | The approvers that are connected, for example you in `agentctl chat` or `agentctl grants listen`. This is the default. |
| `classifier` | A runner that you choose. It answers allow, refuse, or unsure. When it is unsure, a human answers. See [The classifier](#the-classifier). |
| `nobody` | Nobody. Each question is refused at once, and the run can use only what is granted. |

Use `nobody` for bots and other runs that nobody watches. Use `classifier` when you want an agent to keep working without you while each ungranted step still gets checked. Keep `human` when you want to decide each step yourself.

With `human`, a question that arrives while no approver is connected is refused at once. The run carries on and gets an error for that call.

### Where the setting comes from

One runner can serve many chats and bots at the same time, so agent.d works out the setting for each run once, when the run starts. It looks at three values in this order:

1. The runner's default: `approval` in its `[runner.<name>]` section of `grants.toml`. If you leave it out, the default is `human`.
2. The session's value, if the run uses a [chat session](/v0/reference/ctx/sessions) that has one.
3. The value passed when the run starts, as `approval` in `runners.run` or `ctx.run`.

The order from strict to loose is `nobody`, then `human`, then `classifier`. A stricter value always replaces the result so far. A looser value replaces it only when an admin connection starts the run.

For any other caller:

- A looser session value is ignored, and the run keeps the stricter value.
- A looser value passed at the start of the run fails with the error `approval_forbidden`.

Some consequences of these rules:

- A runner whose default is `nobody` stays `nobody` for every bot that uses it, whatever a client asks for.
- A session with a loose setting leaves a stricter runner as it is.
- A run that Lua starts with `ctx.run` is at least as strict as the run that started it.
- If you change a session's setting while a run is busy, the change applies from the next run.

### Admin connections

An admin connection is a client that connects to `/ws` with the admin token. This is the same token `agentctl grants listen` uses: `AGENTD_ADMIN_TOKEN`, or the file `$XDG_STATE_HOME/agentd/admin-token`. Only an admin connection can make a run's approval looser.

Sessions that an admin connection creates belong to admin connections. Other clients cannot see them, run on them, change them, or delete them, so the setting of one chat window stays out of reach of your bots and other clients.

::: warning
With `--no-auth`, there are no tokens. Every client that connects to `/ws` without an interface token is an admin connection. It can see admin sessions and make approval looser for its runs. The daemon prints a warning about this when it starts.
:::

## Answering with `agentctl grants listen`

To answer questions in your terminal, run:

```bash
agentctl grants listen
```

The command connects as an approver and shows each request as it arrives. For each one, choose an answer:

| Answer | What it does |
|---|---|
| `allow_once` | Allows this request only. The next identical request asks again. |
| `allow_forever` | Allows this request and writes a grant to `grants.toml`, so the same request does not ask again. |
| `deny` | Refuses the request. The caller gets an error. |

`agentctl chat` shows the same requests in the chat, and you can answer them there.

### What an answer allows

An allow covers what was asked, and nothing more.

- **Before an action starts** (a missing grant, a runner `allowed_actions` question, or a confirm), the allow covers that one call of the action.
- **During an action** (an ungranted `ctx.fs`, `ctx.http`, `ctx.shell` or similar call), the allow covers that exact request.
  - For a file or a host, the allow lasts until the run ends. The agent can read the same file again or page through the same API without a new question.
  - For a program, the allow covers one run of that command. A command can behave differently after the agent changes files, so agent.d asks each time.
  - The folders and hosts of the [shell sandbox](/v0/security/sandbox) stay as they are. An allow during an action does not add to them.

If the same run asks two identical questions at the same moment, agent.d asks once and both calls get the answer.

### What "allow forever" writes

| Request kind | Change in grants.toml |
|---|---|
| `missing_grant` | Adds the permissions to the `granted` list of the tool or service. |
| `runner_action` | Adds the action to the runner's `allowed_actions` list. |
| `confirm` | Adds the action to `[approval].skip`. |
| `folder` | Adds the folder to `[folders].trusted`. |

For the current run, "allow forever" works like "allow once". Later calls pick up the new grant from the file.

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
| `reason` | A sentence that says what is missing. |
| `missing` | The permissions that are not granted. |
| `detail` | What the call does. For an action, its arguments. For a program, the command (`argv`) and its working directory (`cwd`). |
| `run_id` | The run that asks. Use it to tell two windows apart. |
| `session_id` | The chat session of the run, if it has one. |
| `warning` | A warning to read before "allow forever", for example on an interpreter. |
| `caller` | Who made the call: the `runner`, `interface`, `service`, `session`, and `user`. |

Requests do not show secret values. If the run read a secret with `ctx.secret.get`, agent.d replaces its value with `<secret name>` everywhere in `detail`. It also shortens very long values.

## Folder questions

A chat session can work in a folder on your machine. When a chat starts in a folder you have not trusted yet, agent.d asks whether it may work there. The request has the kind `folder`, and its `action` is the folder's full path.

- "Allow once" trusts the folder for this chat.
- "Always trust" also adds the folder to [`[folders]`](/v0/security/grants#folders), so later chats there start without a question.
- "Deny" starts the chat without a folder.

Folder questions go to a person. The classifier does not answer them, and if no person is connected, the folder is refused.

## The classifier

The classifier is a runner that answers questions for you. Use it when you want an agent to keep working on its own, with a check on each step that you did not grant.

### Set up a classifier

First, register a runner in Lua. Its system prompt tells it what you allow. Give it no actions:

```lua
agentd.runner({
  name   = "approval_classifier",
  model  = "anthropic/claude-opus-4-7",
  system = "Allow reading and building code in the project. Refuse anything that publishes, deploys, or deletes.",
})
```

Then name it in `grants.toml`, grant it its provider, and choose which runners use it:

```toml
[approval]
classifier = "approval_classifier"

[runner.approval_classifier]
granted = ["ai:anthropic"]

[runner.coder]
granted = ["ai:anthropic"]
approval = "classifier"
```

agent.d checks the classifier each time it loads `grants.toml` or your Lua files. If one of these rules is broken, the load fails with a message that says which:

- It is a registered runner.
- It has no actions: none in Lua, none from its skills, and no `allowed_actions` in `grants.toml`.
- It uses a provider that agent.d controls, such as `anthropic` or `openai`. Providers that run their own tools, such as `codex` or `openai-cli`, cannot be used.
- It has the `ai:` grant for its provider.

The load also fails if a runner has `approval = "classifier"` and no classifier is named.

### How the classifier answers

agent.d sends the request to the classifier as data, and the classifier's instructions tell it to ignore any instructions written inside the request. It gives one of three answers, with a short reason:

- **allow**: the call runs one time. The classifier only allows once. It cannot allow forever.
- **refuse**: the call is refused. The error starts with "The classifier refused" and gives its reason.
- **unsure**: a human answers, as if the setting were `human`. The request they see includes the classifier's reason.

The classifier also sees what you wrote. When you start a run through an interface, such as a chat, it gets your last five messages in that session and the run's prompt. If you asked for the action, or said it was fine, it can allow it on that basis. What the agent wrote is left out, since it could carry instructions from files or web pages. Runs started by services and webhooks pass no messages, and a run the agent starts with `ctx.run` passes the messages of the run that started it.

If the classifier takes more than 30 seconds, fails, or gives an answer agent.d cannot read, the answer counts as unsure. The classifier handles up to four questions at a time, in its own slots apart from your other runners, so a busy daemon does not hold it up.

The classifier answers questions about missing grants, confirm actions and programs. Runner questions, where a runner calls an action outside its `allowed_actions` list, go to a human.

The trace records each decision: who decided (`human`, `classifier`, or `nobody`) and the classifier's reason. See [Observability](/v0/operations/observability).

### When no classifier is named

Without a named classifier, you cannot set a session to `classifier`. If a run still ends up with `classifier`, for example because you removed the classifier and reloaded, the run uses `human` and the daemon logs a warning.

## Timeout

If no approver answers within `approval_timeout_ms`, the request is refused. The default is 120 000 ms (2 minutes). To change it, use `--approval-timeout` or `AGENTD_APPROVAL_TIMEOUT_MS`.

After a timeout, the caller gets an error. agent.d does not retry the call; the client can try again.

## Build your own approver

You can write your own approval tool, for example a button in a web app or a message in a team chat. Connect to the `/control` WebSocket endpoint with the admin token, and your client becomes an approver. The server sends each request as an `approval.request` event, and you answer with `approvals.resolve`:

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

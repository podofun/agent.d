# Step 5: Run the Agent

The configuration is complete. You can now start agent.d and review a staged diff.

## Prepare a repository

Use a Git repository that has staged changes. Run `git status` in that repository to confirm the changes.

```bash
git status --short
```

The first column shows the index status. For example, `M ` identifies a staged modification.

The output is similar to this sample:

```text
M  example.txt
```

Your output contains the names of your changed files.

## Start agent.d

Start agent.d in one terminal:

::: code-group
```bash [installed binary]
agentd
```
```bash [current source tree]
cargo run -p daemon
```
:::

By default, agent.d loads `init.lua` and `grants.toml` from `~/.config/agentd`.

The startup output must show one action, one runner, and one skill.

The output is similar to this sample:

```text
  AGENTD v0.8.3-alpha   ready in 3 ms

  ➜  Local   http://127.0.0.1:7777/
  ➜  WS      ws://127.0.0.1:7777/ws
  ➜  Control ws://127.0.0.1:7777/control

  Loaded    1 action, 1 runner, 0 services, 1 skill
  Init      /home/user/.config/agentd/init.lua
```

Your output can show a different version, start time, and home directory.

Keep agent.d active. Open a second terminal for the next commands.

## Verify the connection

Run this command:

```bash
agentctl health
```

The command returns `ok` when `agentctl` can connect to agent.d.

```text
ok
```

## Verify the action

List the registered actions:

```bash
agentctl tools
```

The output must contain `git.diff`.

```text
git.diff
```

Call the action directly:

```bash
agentctl call git.diff
```

The result contains `exit_code` and `diff`. An exit code of `0` means that Git completed the command.

The output is similar to this sample:

```json
{
  "duration_ms": 3,
  "result": {
    "diff": "diff --git a/example.txt b/example.txt\nindex 08fe272..06fcdd7 100644\n--- a/example.txt\n+++ b/example.txt\n@@ -1 +1,2 @@\n first line\n+second line\n",
    "exit_code": 0
  }
}
```

The duration and diff contents depend on your repository.

If `diff` is empty, make sure that the repository has staged changes.

## Run the reviewer

```bash
agentctl runner run backend_reviewer "Review the staged diff."
```

The runner sends the skill instructions to the model. The model can call `git.diff` to get the staged changes.

agent.d verifies the runner permission before it calls the model. It also verifies the action permission before it runs Git.

The final result contains the review text, provider, model, and stop reason.

The output is similar to this sample:

```json
{
  "model": "sonnet",
  "provider": "anthropic-cli",
  "stop_reason": "end_turn",
  "text": "The change adds one line to example.txt. I found no defects."
}
```

The model writes a new review for each diff. Thus, the review text depends on your changes.

To show only the review text, add `--text-only`:

```bash
agentctl runner run backend_reviewer "Review the staged diff." --text-only
```

The output contains only the model text:

```text
The change adds one line to example.txt. I found no defects.
```

## If the runner call fails

Run `claude` in a terminal. Complete its authentication steps.

Then, run the reviewer command again.

Make sure that the `claude` command is on the `PATH` used by agent.d.

If you selected the Anthropic API, make sure that the key is in the agent.d secret store. Refer to [Provider credentials](/v0/providers/credentials).

## What you made

You made a Git action that reads a staged diff. The action does not contain an operation that changes repository files.

The configuration directory can contain more agents, actions, skills, runners, and services. Load their files from the same `init.lua` file.

## What to read next

- [Write tools](/v0/writing/tools) explains actions and input schemas.
- [Write runners](/v0/writing/runners) explains model configuration.
- [Write skills](/v0/writing/skills) explains reusable model instructions.
- [Permissions](/v0/security/grants) explains all grant layers.
- [Development operations](/v0/operations/observability) explains traces and logs.

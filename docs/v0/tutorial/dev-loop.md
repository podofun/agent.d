# Step 6 — The Dev Loop

Stopping and restarting the daemon every time you edit Lua is slow. This page shows the three tools that make iteration fast: `--watch` for hot reload, `agentctl types` for editor autocomplete, and `agentctl trace` for structured log tailing.

## Hot reload with `--watch`

Start the daemon with the `--watch` flag:

::: code-group
```bash [release]
daemon \
  --init ~/projects/git-reviewer/init.lua \
  --grants-file ~/projects/git-reviewer/grants.toml \
  --watch
```
```bash [cargo]
cargo run -p daemon -- \
  --init ~/projects/git-reviewer/init.lua \
  --grants-file ~/projects/git-reviewer/grants.toml \
  --watch
```
:::

The daemon now watches:

- `init.lua` and every file pulled in via `import()`.
- Every skill `.md` file loaded by `agentd.skills.load` or `agentd.skills.dir`.
- `grants.toml`.

When any of these change, the daemon rebuilds the runtime in place. In-flight requests drain on the old runtime; new requests hit the reloaded one. Durable memory survives reloads (it is stored on disk). A connected approval operator on `/control` also survives.

Try it: edit the system prompt in `runners/backend_reviewer.lua` and save. The daemon prints a reload notice within a second, and your next `agentctl runner run` picks up the change immediately.

::: tip No restart needed for grants
If you add a new permission to `grants.toml`, the `--watch` daemon picks it up automatically — no restart required.
:::

## Editor autocomplete with `agentctl types`

agent.d ships LuaLS type stubs so your editor understands `agentd.*` and `ctx.*`. Generate them while the daemon is running:

```bash
agentctl types ~/projects/git-reviewer
```

This writes three files into the project:

| File | Contents |
|------|----------|
| `.luals/agentd.lua` | Core `agentd.*` API stubs. |
| `.luals/project.lua` | Live action, runner, and skill names from the running daemon. |
| `.luarc.json` | LuaLS workspace config that points at both stub files. |

`.luarc.json` is merged if it already exists. After running `agentctl types`, reload your editor's Lua language server and you get completion and type hints for every `agentd.action`, `ctx.shell`, `ctx.log`, and so on.

::: info `--watch` regenerates stubs automatically
When running in `--watch` mode the daemon regenerates the `.luals/` stubs after each reload, so your editor stays in sync as you add new actions and runners.
:::

## Tail structured traces with `agentctl trace`

Every action call, runner invocation, log line, and permission decision is written to a trace file (default `$XDG_STATE_HOME/agentd/trace.jsonl`). Tail it live:

```bash
agentctl trace -f
```

Show the last 50 lines then follow:

```bash
agentctl trace -f -n 50
```

Read from a specific file:

```bash
agentctl trace --file /tmp/my-trace.jsonl -f
```

Use the trace to see exactly what happened during a runner call: which actions were invoked, what they returned, how long each step took, and whether any permission checks fired.

## What to read next

You have built a complete git review agent. From here:

- **[Concepts](/v0/concepts/)** — deep-dive into the runtime model, services, memory, and the permission engine.
- **[Writing components](/v0/writing/tools)** — reference-level guidance for tools, runners, skills, and services.
- **[Recipes](/v0/recipes/)** — worked examples for common patterns like webhooks, Discord bots, and per-user memory.
- **[Security](/v0/security/grants)** — understand grants, approval flows, and the sandbox in detail.

## See also

- [CLI reference](/v0/reference/cli)
- [Operations: observability](/v0/operations/observability)
- [Concepts: runtime](/v0/concepts/runtime)
- [Security: grants](/v0/security/grants)

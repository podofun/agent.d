# Glossary

Alphabetical definitions for every core term in agent.d.

---

**Action** — One callable operation, addressed by its fully-qualified name `tool.action` (e.g. `git.status`). Registered with `agentd.action()`. See [Tools and actions](/v0/concepts/tools-and-actions).

**Approval** — An interactive permission escalation. When an action has `confirm = true`, or the permission engine cannot auto-resolve a grant, it pauses and asks an operator (via `/control`) to allow or deny. See [Approvals](/v0/security/approvals).

**Caller** — The identity carried into every handler invocation. Surfaces as `ctx.caller` in Lua, containing fields like `interface`, `session`, `user`, `runner`, and `execution`. See [Interfaces and callers](/v0/concepts/interfaces-and-callers).

**ctx** — The per-invocation capability handle passed as the second argument to action handlers (first argument to services). It is the only way for Lua code to reach system resources. See [ctx reference](/v0/reference/ctx/).

**Daemon** — The `daemon` binary: the Rust process that loads Lua components, enforces the permission engine, and serves the HTTP + WebSocket API. See [How it works](/v0/guide/how-it-works).

**Grant** — An explicit permission award in `grants.toml` that unlocks one or more permission slugs for a tool, runner, service, interface, or package. See [Permissions & grants](/v0/security/grants).

**Interface** — A client surface that connects to the daemon. Today the only interface is WebSocket (`/ws`). Interfaces have their own allowlist layer in the permission engine. See [Interfaces and callers](/v0/concepts/interfaces-and-callers).

**Memory** — Durable, namespaced key/value storage backed by `redb` (`memory.redb`). Survives daemon restarts and hot reloads. Accessed via `ctx.memory`. Permission slugs: `memory.read:<ns-glob>` / `memory.write:<ns-glob>`. See [Memory and state](/v0/concepts/memory-and-state).

**Package** — A bundle of agent.d components (tools, runners, skills, services) distributed via git, with a `package.toml` manifest that declares its required permission slugs. Installed and managed with `agentctl packages`. See [Managing packages](/v0/packages/managing).

**Permission** — A slug of the form `domain[:specifier]` (e.g. `shell.exec:git`, `net:api.example.com`, `fs.read:/tmp/**`) that represents one class of system access. See [Permission slugs](/v0/security/permission-slugs).

**Policy** — The `[policy]` block in `grants.toml` that holds hard denials (`deny_actions`, `deny_permissions`) and pre-approvals (`auto_confirm`). Policy is the final layer in the five-layer permission engine. See [Permissions & grants](/v0/security/grants).

**Provider** — An AI backend registered in the daemon (e.g. `anthropic`, `openai`, `codex`). Selected via the model string prefix `"<provider>/<model_id>"`. See [Providers](/v0/providers/).

**Runner** — A named AI worker: a model, a merged system prompt built from skills, and an advisory action allowlist. Registered with `agentd.runner()`. See [Runners](/v0/concepts/runners).

**Runtime** — The daemon process as a whole, including the Lua VM, the permission engine, the executor, and all loaded components. See [How it works](/v0/guide/how-it-works).

**Sandbox** — The security boundary that restricts what the daemon process and its Lua handlers can access. See [Sandbox](/v0/security/sandbox).

**Service** — A long-running background Lua task (e.g. a gateway or poller) managed by the daemon with restart supervision. Registered with `agentd.service()`. See [Services](/v0/concepts/services).

**Skill** — Reusable instructions (Markdown text, optionally with YAML frontmatter) composed into a runner's system prompt. Loaded with `agentd.skills.load()` / `agentd.skills.dir()`. See [Skills](/v0/concepts/skills).

**State** — Ephemeral in-memory key/value storage scoped to the current runtime lifetime. Lost on restart or hot reload. Accessed via `ctx.state`. No permission required. See [Memory and state](/v0/concepts/memory-and-state).

**Tool** — A namespace that groups related actions (e.g. the `git` tool owns `git.status`, `git.diff`). Registered with `agentd.tool()`. See [Tools and actions](/v0/concepts/tools-and-actions).

**Trace** — The JSONL event log written to `$XDG_STATE_HOME/agentd/trace.jsonl`. Tailed with `agentctl trace`. See [Observability](/v0/operations/observability).

---

## See also

- [Core concepts overview](/v0/concepts/)
- [How it works](/v0/guide/how-it-works)
- [Permission slugs](/v0/security/permission-slugs)
- [ctx reference](/v0/reference/ctx/)

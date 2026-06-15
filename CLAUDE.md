# CLAUDE.md

This file is the operating guide for Claude Code when working in this
repository. Treat it as project documentation, not as a scratchpad.

The goal of this document is to make code changes consistent with the design of
agentd. Keep it accurate when behavior changes. Remove stale statements instead
of adding exceptions around them.

## Project Summary

agentd is a portable runtime for tool-using AI runners. It is a Rust daemon with
a sandboxed Lua userland. The daemon loads tools, actions, runners, skills,
services, memory, model providers, authentication, permissions, and interfaces
once, then exposes them to clients through a local API.

agentd is not a chatbot application and it is not a web framework. It is the
runtime layer that lets the same runner definitions and tool permissions work
from a command-line client, chat integration, webhook, editor integration, or
small server.

The implementation bias is:

- Rust owns enforcement, state, scheduling, transport, process execution,
  credential access, filesystem access, network access, tracing, and public API
  boundaries.
- Lua owns user-defined behavior: tool registration, action handlers, runner
  definitions, skill loading, service bodies, event glue, and small workflow
  composition.

Default to Lua for behavior that does not need to be enforced. Move logic into
Rust when the runtime must guarantee safety, isolation, durability, scheduling,
or protocol correctness.

## Required Vocabulary

Use these terms consistently in code, documentation, tests, and user-facing
messages:

| Term | Meaning |
| --- | --- |
| Runtime | The daemon process that loads components and enforces policy. |
| Runner | A named AI worker with a model, system instructions, skills, and allowed actions. |
| Skill | Reusable runner instructions. A skill is Markdown with frontmatter or an inline Lua definition. |
| Tool | A named collection of actions and declared host capability requirements. |
| Action | One callable operation exposed by a tool, such as `git.status`. |
| Service | A long-running named Lua task supervised by the executor. |
| Auth | Credential declaration, storage, and delivery to approved runtime components. |
| Permission | A runtime-enforced grant such as `shell.exec:git` or `net:api.example.com`. |
| Memory | Durable namespaced key/value storage exposed through `ctx.memory`. |
| Interface | A client surface that talks to the runtime. WebSocket is the supported transport. |
| Execution | One action call, runner call, service start, or background task with trace output. |

Avoid obsolete terms in new code. Do not use "agent" when the code means
`Runner`.

## Repository Layout

This is a Cargo workspace using resolver `3`, Rust edition `2024`, and a
minimum supported Rust version of `1.85`.

Each crate maps to one runtime concept. Keep that boundary intact. A new core
concept should usually become a new crate. A new client surface should become a
new interface crate or an external client that speaks WebSocket to the daemon.

| Crate | Responsibility |
| --- | --- |
| `agentd-types` | Shared DTOs and traits. No logic and no I/O. |
| `agentd-trace` | Structured trace events and trace sinks. |
| `agentd-secrets` | Credential store trait, memory store, and OS keyring store. |
| `agentd-codex` | `codex app-server` subprocess transport over JSON-RPC stdio. |
| `agentd-ai` | Provider trait, provider registry, request and response types, built-in providers. |
| `agentd-permissions` | Default-deny permission engine and `grants.toml` loading. |
| `agentd-approvals` | Transport-independent interactive approval broker. |
| `agentd-skills` | Skill definitions and Markdown skill loading. |
| `agentd-runners` | Runner definitions, runner registry, and prompt composition. |
| `agentd-packages` | Git-installed package manifests, package index, and grant desugaring. |
| `agentd-mcp` | Per-invocation MCP loopback for provider-owned tool loops. |
| `agentd-services` | Service definitions, service registry, and service status storage. |
| `agentd-shell` | Argv-only process execution primitive. |
| `agentd-fs` | Filesystem primitive functions. |
| `agentd-memory` | Durable key/value memory trait, redb backend, and test backend. |
| `agentd-http` | HTTP client primitive and host permission helper. |
| `agentd-ws` | WebSocket client primitive and host permission helper. |
| `agentd-cli` | `agentctl` console client. |
| `agentd-scripting` | Lua host, Lua sandbox, action registry, service registry, and `ctx` bindings. |
| `agentd-executor` | Execution kernel for actions, runners, services, traces, permissions, and approvals. |
| `agentd-api` | Axum WebSocket API and health endpoint. |
| `daemon` | Runtime binary and configuration wiring. |

Dependency direction matters. `agentd-types` is the leaf crate. `daemon` is the
root binary. Do not introduce dependency cycles.

## Architectural Rules

Keep behavior in the crate that owns the concept.

- `agentd-types` must remain data and traits only.
- `agentd-permissions` owns authorization decisions. Other crates may ask it
  for a decision but must not duplicate its rules.
- `agentd-executor` owns execution flow. It does not depend on Lua.
- `agentd-scripting` owns Lua registration, sandboxing, and Lua-facing host
  capabilities. It does not decide global policy.
- `agentd-api` owns transport envelopes and WebSocket routing. It must not
  execute actions directly.
- `daemon` wires components together. It must not accumulate business logic.
- Primitive crates such as `agentd-shell`, `agentd-fs`, `agentd-http`,
  `agentd-ws`, `agentd-memory`, and `agentd-secrets` do the requested primitive
  operation. Permission checks happen in the caller that has execution context.

When adding a feature, identify the enforcement boundary before writing code.
If the runtime must prevent misuse, the check belongs in Rust and must be
covered by tests.

## Runtime Configuration

The daemon evaluates one Lua entry point, `init.lua`. The path comes from
`runtime.init`, `--init`, or `AGENTD_INIT`. The default path is
`$XDG_CONFIG_HOME/agentd/init.lua`.

Configuration sources are resolved in this order:

1. Command-line flags.
2. Environment variables read by clap.
3. `$XDG_CONFIG_HOME/agentd/config.toml`.
4. `RUST_LOG`, for logging only.
5. Built-in defaults.

Important daemon flags and environment variables:

| Flag | Environment variable | Purpose |
| --- | --- | --- |
| `--config` | `AGENTD_CONFIG` | Path to `config.toml`. |
| `--init` | `AGENTD_INIT` | Path to the Lua entry file. |
| `--addr` | `AGENTD_ADDR` | HTTP and WebSocket bind address. |
| `--trace-file` | `AGENTD_TRACE_FILE` | JSONL trace file path. |
| `--log` | `AGENTD_LOG` | Tracing filter. |
| `--grants-file` | `AGENTD_GRANTS_FILE` | Path to `grants.toml`. |
| `--token` | `AGENTD_TOKEN` | Bearer token for `/ws`. |
| `--no-auth` | `AGENTD_NO_AUTH` | Disable authentication for development. |
| `--admin-token` | `AGENTD_ADMIN_TOKEN` | Bearer token for `/control`. |
| `--approval-timeout-ms` | `AGENTD_APPROVAL_TIMEOUT_MS` | Interactive approval timeout. |

Runtime defaults:

- Bind address: `127.0.0.1:7777`.
- Trace file: `$XDG_STATE_HOME/agentd/trace.jsonl`.
- Grants file: `$XDG_CONFIG_HOME/agentd/grants.toml`.
- Public token file: `$XDG_STATE_HOME/agentd/token`.
- Admin token file: `$XDG_STATE_HOME/agentd/admin-token`.
- Memory database: `$XDG_DATA_HOME/agentd/memory.redb`.
- Package root: `$XDG_DATA_HOME/agentd/packages`.

When authentication is enabled and a token is not configured, the daemon mints
the token and writes it with mode `0600`.

## API Surface

The daemon exposes one health endpoint and two WebSocket planes.

| Endpoint | Authentication | Purpose |
| --- | --- | --- |
| `GET /health` | None | Liveness probe. Returns `ok`. |
| `/ws` | Public bearer token | Data plane for clients. |
| `/control` | Admin bearer token | Operator plane for approvals. |

The WebSocket envelope is:

```json
{ "id": 1, "method": "actions.call", "params": {} }
```

Responses use the same `id`:

```json
{ "id": 1, "ok": true, "result": {} }
```

Error responses include `error` and `code`:

```json
{ "id": 1, "ok": false, "error": "denied", "code": "denied" }
```

Public methods:

- `health`
- `tools.list`
- `actions.call`
- `runners.list`
- `runners.inspect`
- `runners.run`
- `skills.list`
- `skills.inspect`
- `services.list`

Control methods:

- `approvals.subscribe`
- `approvals.resolve`

The control socket also receives server-pushed `approval.request` frames.

Do not add HTTP action routes. The action and runner API is WebSocket-only.

## Permission Model

agentd is default-deny. A Lua manifest declares what a component wants. The
manifest never grants the permission.

The permission decision is the intersection of:

```text
tool grants
action requirements
runner allowed actions
interface allowed actions
service allowed actions
policy denylist and confirmation policy
```

The only source of grants is `grants.toml`.

Permission slugs have the shape `domain[:specifier]`. Wildcards are allowed in
the specifier:

- `shell.exec:git`
- `shell.exec:*`
- `net:api.example.com`
- `net:*`
- `fs.read:/tmp/**`
- `fs.write:/tmp/project/*`
- `secret:anthropic_api_key`
- `secret:openai_*`
- `memory.read:discord/**`
- `memory.write:*`
- `ai:anthropic`
- `shell.unrestricted`

`shell.unrestricted` is a plain non-wildcard grant. Holding it makes `ctx.shell`
skip the native shell sandbox and run the child on the host shell. The engine
treats it as an ordinary slug; the sandbox-skip meaning is interpreted in the
shell binding (see the native shell sandbox under `ctx.shell`).

Example:

```toml
[tool.git]
granted = ["shell.exec:git"]

[runner.backend_reviewer]
allowed_actions = ["git.diff", "git.status"]

[interface.telegram]
allowed_actions = ["git.status"]

[policy]
deny_actions = ["shell.exec"]
deny_permissions = []
auto_confirm = []
```

Action requirements can ask for confirmation:

```lua
agentd.action({
  name = "calendar.delete_event",
  requires = { "calendar.write" },
  confirm = true,
  handler = function(args, ctx)
    return { deleted = args.id }
  end,
})
```

Interactive approval is available only for escalatable denials. Missing tool
grants and `confirm = true` actions are escalatable. Policy denials, runner
allowlist denials, interface allowlist denials, and service allowlist denials
are hard denials.

Approval verdicts:

| Verdict | Effect |
| --- | --- |
| `allow_once` | Allows the current dispatch through an in-memory overlay. |
| `allow_forever` | Appends the grant or auto-confirm rule to `grants.toml` and reloads the permission engine. |
| `deny` | Rejects the dispatch. |

If no approver is connected or the approval request times out, the broker fails
closed.

## Packages

A package is a git-installed bundle of tools, actions, runners, services, and a
single declared permission set.

Installed packages live under:

```text
$XDG_DATA_HOME/agentd/packages/<package-name>
```

Package provenance and commit pins live in:

```text
$XDG_DATA_HOME/agentd/packages/index.toml
```

Packages are loaded from Lua with a bare package name:

```lua
import("acme")
```

Relative Lua files are loaded with paths:

```lua
import("tools/git.lua")
```

Package registration names are automatically prefixed:

- Tool `git` in package `acme` becomes `acme/git`.
- Action `git.status` in package `acme` becomes `acme/git.status`.
- Runner `reviewer` in package `acme` becomes `acme/reviewer`.
- Service `poller` in package `acme` becomes `acme/poller`.

Package trust is explicit:

```toml
[package.acme]
trusted = true
```

Trusted packages are desugared into ordinary tool, runner, and service grant
rows by `agentd-packages`. The permission engine is not modified for packages.
Without `trusted = true`, a package can register components but contributes no
grants.

## Model Providers

`agentd-ai` defines one provider trait and common request and response types.
Providers are registered by name and runners select a model with:

```text
<provider>/<model_id>
```

Examples:

- `anthropic/claude-sonnet-4-5`
- `anthropic-cli/claude-sonnet-4-5`
- `codex/gpt-5-codex`

Provider loop modes:

| Loop mode | Meaning |
| --- | --- |
| `ExecutorOwned` | The executor drives the model tool-use loop, dispatches tool calls through actions, and re-prompts until text is returned or the turn limit is reached. |
| `ProviderOwned` | The provider owns its own tool-use loop and reaches actions through the MCP loopback. |

Built-in providers:

- `MockProvider` for deterministic tests.
- `ClaudeApiProvider` for Anthropic Messages API.
- `ClaudeCliProvider` for `claude -p` with MCP loopback.
- `CodexAppServerProvider` for `codex app-server` over JSON-RPC stdio.
- `CodexCliProvider` for text-only `codex exec`.
- `OpenAiApiProvider` for OpenAI-compatible API work.

Provider-owned tool calls must still pass through `agentd-mcp` and the executor
dispatcher. Do not let a provider call host tools directly.

## Lua Userland

Lua is the userland configuration and behavior layer. `agentd-scripting`
creates one Lua state, installs the `agentd` API, installs helper globals, then
locks down unsafe standard libraries.

The Lua sandbox removes `io`, `os`, `package`, `debug`, `require`, `dofile`,
`loadfile`, `load`, `loadstring`, `collectgarbage`, raw metatable escape
functions, and metatable mutation functions. Safe standard libraries remain:
`string`, `table`, `math`, `coroutine`, `utf8`, and safe base functions.

The daemon evaluates only `init.lua`. Additional files must be loaded with the
sandboxed `import` function.

### Lua Entry Point

```lua
import("tools/git.lua")

agentd.skills.dir("skills")

agentd.skill({
  name = "terse",
  description = "Short direct answers.",
  system = "Reply in plain text. Keep the answer short.",
})

import("runners/backend_reviewer.lua")
```

`import(path)` resolves relative to the directory that contains `init.lua`.
Absolute paths and `..` traversal are rejected. Repeated imports of the same
canonical file are ignored.

### Tool Registration

```lua
agentd.tool({
  name = "git",
  requires = { "shell.exec" },
})
```

`requires` declares the host permissions the tool expects. It does not grant
permissions.

### Action Registration

Preferred table form:

```lua
agentd.action({
  name = "git.status",
  requires = { "shell.exec" },
  handler = function(args, ctx)
    local result = ctx.shell("git", { "-C", args.cwd or ".", "status", "--short" })
    return {
      stdout = result.stdout,
      stderr = result.stderr,
      exit_code = result.exit_code,
    }
  end,
})
```

Short form:

```lua
agentd.action("git.status", function(args, ctx)
  return ctx.shell("git", { "status", "--short" })
end)
```

Use the table form when an action has `requires`, `confirm`, or other metadata.

### Skill Registration

Inline:

```lua
agentd.skill({
  name = "reviewer",
  description = "Reviews code for correctness.",
  system = "Focus on bugs, regressions, missing tests, and unclear behavior.",
  actions = { "git.diff", "git.status" },
})
```

Markdown file:

```markdown
---
name: reviewer
description: Reviews code for correctness.
actions:
  - git.diff
  - git.status
---

Focus on bugs, regressions, missing tests, and unclear behavior.
```

Load one file:

```lua
agentd.skills.load("skills/reviewer.md")
```

Load a directory:

```lua
agentd.skills.dir("skills")
```

The Markdown body becomes the system-prompt fragment. The `actions` field is an
advisory allowlist that is merged into runner allowed actions during runner
composition.

### Runner Registration

```lua
agentd.runner({
  name = "backend_reviewer",
  model = "anthropic/claude-sonnet-4-5",
  skills = { "reviewer", "terse" },
  actions = { "git.diff", "git.status" },
  system = "Review only backend changes.",
})
```

The provider name is the prefix before `/` in `model`. Runner composition merges
skill system text, runner system text, skill actions, and runner actions.

### Service Registration

Basic form:

```lua
agentd.service("poller", function(ctx)
  while true do
    ctx.log.info("polling")
    sleep(60000)
  end
end)
```

With restart policy:

```lua
agentd.service("poller", {
  restart = "on_failure",
  backoff_ms = 1000,
  backoff_max_ms = 30000,
}, function(ctx)
  while true do
    ctx.log.info("polling")
    sleep(60000)
  end
end)
```

Supported restart values are `always` and `on_failure`.

### Invocation Context

Action handlers and service bodies receive `ctx` as an argument. `ctx` is never
a global. Every privileged `ctx` binding performs an inline permission check
against the current execution's effective grants.

There are two permission gates:

1. Executor preflight checks action-level requirements.
2. `ctx` bindings check each host capability call.

Recursive action calls inherit the caller identity. They do not replace the
outer `Caller`.

### `ctx.log`

```lua
ctx.log.trace("message")
ctx.log.debug("message")
ctx.log.info("message")
ctx.log.warn("message")
ctx.log.error("message")
```

Logging is always allowed.

### `ctx.shell`

Short form:

```lua
local result = ctx.shell("git", { "status", "--short" })
```

Structured form:

```lua
local result = ctx.shell({
  bin = "git",
  args = { "-C", "/tmp/repo", "status", "--short" },
  cwd = "/tmp/repo",
  stdin = "",
  separate_stderr = true,
})
```

Required permission:

- `shell.exec` for any executable.
- `shell.exec:<bin>` for one executable.

The shell primitive is argv-only. Do not add a shell interpreter path for
compound commands.

#### Native shell sandbox

Every `ctx.shell` child runs inside a native OS sandbox (NativeShellSandbox)
enforced on the child process: Landlock on Linux, `sandbox-exec` (Seatbelt) on
macOS, a restricted token + firewall/WFP on Windows. The sandbox rules are
derived from the execution's effective grants, so the OS confines the child to
the same filesystem and network the permission engine already allows.

Filesystem enforcement:

- Writes: denied except under `fs.write` grants (globs collapsed to the concrete
  ancestor directory) plus `/dev/null` scratch.
- Reads: restricted to `fs.read` grants, the writable subtrees, and a system read
  baseline (`/usr`, `/bin`, `/lib`, `/lib64`, `/etc`, `/opt`, `/proc/self`,
  common `/dev` nodes) so the binary and its libraries load.

Network enforcement (host-granular):

- A child reaches a host iff a `net:<host>` grant covers it. Network flows
  through an in-process egress proxy (in `agentd-shell`) that reads the
  destination host from the TLS SNI or HTTP `Host`/`CONNECT` target (no TLS
  termination, no MITM) and checks it with `Permission::covers`.
- The child is confined so the proxy is its ONLY route out: on Linux a rootless
  network namespace whose sole egress is the proxy (via an in-netns supervisor +
  SCM_RIGHTS fd passing); on macOS Seatbelt allowing only the proxy's loopback
  port; on Windows a sandbox user + firewall/WFP allowing only the proxy port.
  Proxy env (`HTTP_PROXY` etc.) is a convenience for cooperating tools — the OS
  confinement, not the env, is the enforcement boundary.
- `allow_net` is the master switch: zero `net:` grants ⇒ no network at all.
- IP-literal / no-SNI / encrypted ClientHello (ECH) destinations are denied.

If no enforcing backend is available on the platform, `ctx.shell` fails closed —
it does not run the child unsandboxed. The only opt-out is the
`shell.unrestricted` grant, which runs the child on the host shell with no
sandbox.

### `ctx.tools`

```lua
local tools = ctx.tools()
```

Returns the registered action catalog. No permission is required.

### `ctx.call`

```lua
local result = ctx.call("git.status", { cwd = "/tmp/repo" })
```

`ctx.call` invokes another action. The inner action requirements are checked.
Actions marked `confirm = true` are rejected through this path.

### `ctx.fs`

```lua
local text = ctx.fs.read("/tmp/input.txt")
ctx.fs.write("/tmp/output.txt", text)
ctx.fs.append("/tmp/output.txt", "\n")
local ok = ctx.fs.exists("/tmp/output.txt")
local stat = ctx.fs.stat("/tmp/output.txt")
local entries = ctx.fs.list_dir("/tmp")
ctx.fs.remove("/tmp/output.txt")
```

Required permissions:

- `fs.read:<absolute-path>` for `read`, `exists`, `stat`, and `list_dir`.
- `fs.write:<absolute-path>` for `write`, `append`, and `remove`.

Relative paths are resolved against the current working directory before
permission slug derivation.

### `ctx.http`

```lua
local response = ctx.http.get("https://api.example.com/items")

local created = ctx.http.post(
  "https://api.example.com/items",
  { name = "example" },
  { headers = { ["x-client"] = "agentd" } }
)

local raw = ctx.http.request({
  method = "PUT",
  url = "https://api.example.com/items/1",
  headers = { ["content-type"] = "application/json" },
  json = { name = "updated" },
  timeout_ms = 10000,
})
```

Client helper:

```lua
local api = ctx.http.client({
  base_url = "https://api.example.com",
  headers = { authorization = "Bearer token" },
  timeout_ms = 10000,
})

local response = api:get("/items")
```

Required permission:

- `net:<host>` derived from the URL host.

### `ctx.ws`

```lua
local ws = ctx.ws.connect("wss://gateway.example.com")
ws:send("hello")
local frame = ws:recv(30000)
ws:close()
```

Frame shape:

```lua
{
  kind = "text",
  text = "payload"
}
```

Helpers:

```lua
local text = ws:recv_text(30000)

ws:each(function(frame)
  ctx.log.info(frame.kind)
end)

local heartbeat = ctx.ws.connect("wss://gateway.example.com", {
  heartbeat_ms = 30000,
  heartbeat = "ping",
})
```

Required permission:

- `net:<host>` derived from the URL host.

### `ctx.secret`

```lua
local value = ctx.secret.get("anthropic_api_key")
ctx.secret.set("service_token", "secret-value")
ctx.secret.delete("service_token")
local exists = ctx.secret.exists("anthropic_api_key")
local keys = ctx.secret.list()
```

Required permissions:

- `secret:<key>` for `get`, `set`, `delete`, and `exists`.
- `secret:*` for `list`.

`exists` never exposes the secret value.

### `ctx.memory`

```lua
local memory = ctx.memory.create("discord/session-123")

local value = memory:get("last_message_id", nil)
memory:set("last_message_id", "42")
local exists = memory:exists("last_message_id")
local keys = memory:keys()
memory:delete("last_message_id")
memory:clear()
```

Required permissions:

- `memory.read:<namespace>` for `get`, `exists`, and `keys`.
- `memory.write:<namespace>` for `set`, `delete`, and `clear`.

`ctx.memory.create(ns)` does not require permission. Operations on the handle do
require permission.

### `ctx.caller`

```lua
local interface = ctx.caller.interface
local runner = ctx.caller.runner
local service = ctx.caller.service
local session = ctx.caller.session
local user = ctx.caller.user
```

Caller fields are read-only strings or `nil`. No permission is required.

Use `ctx.caller.session` for per-conversation memory keys when a client supplies
stable sessions.

### `ctx.ai`

```lua
local answer = ctx.ai.ask("Summarize this text.", {
  model = "anthropic/claude-sonnet-4-5",
  max_tokens = 500,
})

local completion = ctx.ai.complete({
  model = "claude-sonnet-4-5",
  system = "Be precise.",
  prompt = "Explain the change.",
  max_tokens = 500,
})

local providers = ctx.ai.providers()
```

Required permission:

- `ai:<provider>`.

The provider is the prefix before `/` in `model`. If the model omits a provider
prefix, the default provider is used before the permission slug is checked.

### `ctx.run`

```lua
local result = ctx.run("backend_reviewer", "Review this diff.")
local text = result.text
```

Structured form:

```lua
local result = ctx.run("backend_reviewer", {
  prompt = "Review this diff.",
  system = "Focus on correctness.",
  model = "anthropic/claude-sonnet-4-5",
  messages = {
    { role = "user", content = "Review this diff." },
  },
})
```

`history` is accepted as an alias for `messages`.

Runner execution is routed through the executor by `RunnerDispatcher`. The
runner's action allowlist and the normal permission engine still apply.

The result table contains:

- `text`
- `provider`
- `model`
- `stop_reason`

### `ctx.structured`

```lua
local verdict = ctx.structured("scorer", {
  prompt = json.encode(payload),
  system = "Return one JSON object with a scores field.",
  retries = 2,
  validate = function(value)
    if type(value.scores) ~= "table" then
      return false, "missing scores"
    end
    return true
  end,
})
```

`ctx.structured` runs a runner through `ctx.run`, strips Markdown code fences,
decodes JSON, optionally validates the decoded table, and retries with the
validation error when the response is invalid. It returns the decoded table and
the original runner response.

### `ctx.state`

```lua
local value = ctx.state.get("key", nil)
ctx.state.set("key", { count = 1 })
ctx.state.delete("key")
local keys = ctx.state.keys()
ctx.state.clear()
```

`ctx.state` is process-wide JSON state. It is not durable memory. Use
`ctx.memory` for durable state.

### Async, Await, Sleep, Timer, Channel, And Parallel

```lua
local handle = async(function()
  sleep(1000)
  return "done"
end)

local value = await(handle)
```

Timers:

```lua
local once = timer.after(1000, function()
  return "done"
end)

local repeating = timer.every(5000, function()
  ctx.log.info("tick")
end)
```

Channels:

```lua
local inbox = channel("events")

async(function()
  inbox:send({ kind = "ready" })
end)

local message = inbox:recv()
```

`channel()` creates an anonymous channel. `channel("name")` returns a named
process-wide channel. Message bodies are copied as Lua tables; do not add JSON
encode/decode round-trips for channel messages.

Parallel helpers:

```lua
local results = parallel({
  function() return ctx.run("reviewer_a", { prompt = "Review A" }) end,
  function() return ctx.run("reviewer_b", { prompt = "Review B" }) end,
}, {
  limit = 2,
  settled = true,
})

local mapped = parallel_map({ "a", "b" }, function(item, index)
  return item .. tostring(index)
end)
```

`parallel` runs functions in separate `async` coroutines and returns results in
input order. `limit` caps live branches. `settled = true` returns `{ ok, value,
error }` entries instead of raising the first branch error.

The cooperative scheduler drives action handlers, service bodies, and
`async(fn)` callbacks as Lua coroutines. Yieldable bindings release the Lua
mutex while I/O runs.

### JSON Helper

```lua
local encoded = json.encode({ ok = true })
local decoded = json.decode(encoded)

local null_value = json.null
local is_null = json.is_null(null_value)

local legacy = json.decode(encoded, { nulls = "nil" })
```

`json.null` preserves JSON null values unless the caller explicitly requests
`nulls = "nil"`.

### String Helpers

The stock `string` table is extended. Helpers work with method-call syntax:

```lua
local value = ("  abc  "):trim()
local left = ("  abc"):ltrim()
local right = ("abc  "):rtrim()
local starts = ("abcdef"):startswith("abc")
local ends = ("abcdef"):endswith("def")
local contains = ("abcdef"):contains("cd")
local parts = ("a,b,c"):split(",")
local words = ("a b c"):split()
```

`contains` and `split` use plain text matching, not patterns.

### Protected Calls

```lua
local ok, value_or_error = pcall(function()
  return ctx.fs.read("/tmp/file")
end)
```

The helper is the short protected-call form exposed in the sandbox.

## Provider Tool Use And MCP Loopback

`agentd-mcp` binds a temporary HTTP JSON-RPC server on `127.0.0.1:0` for a
single runner invocation. The server exposes the runner's allowed action catalog
as MCP tools. Each MCP `tools/call` runs through `agentd_types::Dispatcher`,
which is implemented by the executor, so the same permission engine fires as a
normal `actions.call`.

`ClaudeCliProvider` uses the loopback with `claude -p`, a generated MCP config,
and `--allowedTools "mcp__agentd__*"`.

`CodexAppServerProvider` uses `codex app-server --listen stdio://`, disables
Codex built-in tools for the thread, configures the agentd MCP server, and
keeps Codex tool access MCP-only. Approval bridging remains defense in depth
for future Codex behavior.

If a provider requests host capabilities outside the MCP action path, route that
request through `Dispatcher::check_grants` or deny it. Do not bypass the
executor.

## Trace Behavior

The executor emits trace events for action dispatch, runner execution, service
lifecycle events, and permission outcomes. The default trace sink appends JSONL
to `$XDG_STATE_HOME/agentd/trace.jsonl`.

Trace output is operational data. Keep event shapes structured and stable.

## Command-Line Client

Build:

```bash
cargo build -p agentd-cli
```

Common commands:

```bash
agentctl health
agentctl tools
agentctl call git.status
agentctl call git.status -d cwd=/tmp
agentctl call git.status --json '{"cwd":"/tmp"}'
agentctl call git.status --result-only --compact
agentctl trace -n 20
agentctl trace -f
agentctl grants listen
agentctl runner ls
agentctl runner inspect backend_reviewer
agentctl runner run backend_reviewer "review this diff"
agentctl runner run backend_reviewer "review this diff" --text-only
agentctl skills ls
agentctl skills inspect reviewer
agentctl services ls
agentctl packages ls
agentctl packages install <git-url> --ref <tag>
agentctl packages update <name>
agentctl packages remove <name>
```

Package commands are local filesystem and git operations. They do not call the
daemon WebSocket API.

`agentctl health` uses HTTP `/health`. Other daemon operations use WebSocket.

## Development Commands

```bash
cargo build
cargo build -p daemon
cargo run -p daemon -- --init examples/init.lua --grants-file examples/grants.toml
cargo run -p agentd-cli -- health
cargo run -p agentd-cli -- tools
cargo test --workspace
cargo test -p agentd-executor
cargo test <test-name>
cargo clippy --workspace --all-targets
cargo fmt
```

Live integration tests are gated by environment variables:

| Environment variable | Effect |
| --- | --- |
| `AGENTD_TEST_KEYRING=1` | Runs OS keyring smoke tests. |
| `AGENTD_TEST_CLAUDE=1` | Runs live Claude CLI tests. |
| `AGENTD_TEST_CODEX=1` | Runs live Codex transport and provider tests. |

Do not enable live model tests unless the task requires them.

## Coding Standards For This Repository

Follow existing crate patterns before introducing new abstractions.

Use structured Rust types for protocol data, permission data, and configuration
data. Avoid ad hoc string parsing when a parser or typed representation already
exists.

Keep public errors specific enough for callers to distinguish denial, missing
items, invalid input, invocation failure, transport failure, and provider
failure.

Do not make primitive crates enforce permissions. They do not have enough
caller context.

Do not add runtime behavior to `daemon` when the behavior belongs to a library
crate.

Do not add Lua globals for privileged host access. Privileged access belongs
under `ctx`.

Do not make a tool manifest self-granting. Grants come from `grants.toml`.

Do not bypass the executor for action execution, runner tool use, service
supervision, approval handling, or permission checks.

Do not introduce project dot-directories for runtime defaults. Use XDG paths.

Do not add shell-string execution. Process execution uses binary plus argv.

When changing security-sensitive behavior, add tests for allowed, denied, and
escalatable cases.

When changing Lua-facing behavior, update the Lua reference in this file and add
tests in `agentd-scripting` or the crate that owns the Rust behavior.

When changing WebSocket behavior, update the method list or envelope
description in this file and test the API crate or CLI path.

When changing package behavior, preserve the design that package trust desugars
into ordinary grants. Do not special-case package trust inside the permission
engine.

## Documentation Rules

Documentation in this repository should use direct names instead of indirect
phrases. Prefer "the executor" over "it" when the antecedent may be unclear.
Prefer "WebSocket data plane" over "the public side".

Do not repeat the same rule in multiple sections. Put the rule in the section
that owns it and link the surrounding explanation to that rule by name.

Update examples when APIs change. A documented example should compile or run
unless it is explicitly labeled as a shape example.

Keep CLAUDE.md focused on repository-wide guidance and the Lua userland
reference. Put crate-local implementation details in that crate's README or
module documentation when the detail is not needed by every future code change.

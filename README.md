<div align="center">

<img src="assets/agentd_logo.png" alt="agentd" width="160" height="160" />

# agent.d

**A portable runtime for tool-using AI agents.**

</div>

agentd is a small runtime for building AI agents that need to call tools safely.
It gives you one place to define what an agent can do. That includes its model,
memory, external access and approval rules.

Use it when the same agent setup should work from more than one surface: CLI
tools, chat integrations, webhooks, editors or small servers. Instead of
rebuilding tools and permissions for each interface, you run agentd once and
connect clients to it.

## Why It Exists

Most agent prototypes start simple: a prompt, a model and a few tool calls.
They become harder to operate when you need to answer questions like:

- Which commands is this agent allowed to run?
- Can this chat bot use the same tools as the CLI?
- How do I stop one integration from getting access intended for another?
- Where should operational state live?
- How do I swap model providers without rewriting every tool?

agentd turns those concerns into runtime configuration instead of application
glue. The daemon loads the runtime components once, then makes them available to
every connected interface.

## How It Helps

- **Define tools once.** Expose actions such as `git.status`, `github.read_pr`
  and `deploy.preview`, then call them from any connected client.
- **Control access centrally.** `grants.toml` decides which components can use
  each capability.
- **Fail closed by default.** A tool cannot touch the local system, network,
  secrets or models unless it has an explicit grant.
- **Ask for approval when needed.** A privileged operator can approve a missing
  grant once or persist it for future runs.
- **Keep agent behavior portable.** Frontends reuse the same agent definitions
  instead of carrying their own copies.
- **Keep operations visible.** The runtime writes structured trace events so
  tool calls and runner activity can be inspected later.
- **Use different providers.** Built-in providers cover Anthropic,
  OpenAI-compatible APIs, local CLI backends and Codex app-server and more to come.

## How It Works

agentd starts by loading an entry file. It registers components, applies grants
and opens a local server.

```text
init.lua + packages + skills
        |
        v
agentd daemon
        |
        +-- /health   liveness check
        +-- /ws       client data plane
        +-- /control  privileged approval plane
```

A typical request looks like this:

1. A client asks the daemon to call an action, for example `git.status`.
2. The runtime finds the tool that owns that action.
3. The runtime checks the action requirements against `grants.toml`, caller
   allowlists and policy.
4. If the call is allowed, the action receives a `ctx` handle for approved host
   capabilities.
5. The result is returned to the client and written to the trace log.

If a required grant is missing, the call is denied unless interactive approvals
are enabled and an operator approves it.

## Core Concepts

| Term | Meaning |
| --- | --- |
| Runtime | The daemon process that loads components and enforces policy. |
| Tool | A package of callable actions. |
| Action | One operation exposed by a tool, such as `git.status`. |
| Runner | A named AI worker with a model, skills and allowed actions. |
| Skill | Reusable instructions that shape runner behavior. |
| Service | A long-running background task, such as a gateway or poller. |
| Permission | A grant such as `shell.exec:git`, `net:api.example.com` or `fs.read:/tmp/**`. |
| Memory | Durable namespaced key/value storage available to actions and services. |
| Interface | A client surface that talks to the daemon. WebSocket is supported today. |

## Installation

Build from source with Rust 1.85 or newer:

```bash
git clone https://github.com/MrF0o/agent.d
cd agent.d
cargo build --release
```

The release build produces:

- `target/release/daemon` - the runtime server.
- `target/release/agentctl` - the console client.

During development, you can run both binaries through Cargo.

## Quick Start

Start the daemon with the bundled example:

```bash
cargo run -p daemon -- \
  --init examples/init.lua \
  --grants-file examples/grants.toml
```

This loads:

- `examples/init.lua`, which registers the example components.
- `examples/tools/git.lua`, which defines Git actions.
- `examples/grants.toml`, which allows the Git tool to run the `git` binary.

The daemon listens on `127.0.0.1:7777` by default.

In another terminal, check that it is alive:

```bash
cargo run -p agentd-cli -- health
```

List the actions the daemon loaded:

```bash
cargo run -p agentd-cli -- tools
```

Call the example Git action:

```bash
cargo run -p agentd-cli -- call git.status
```

Follow the runtime trace:

```bash
cargo run -p agentd-cli -- trace -f
```

With release binaries on your `PATH`, use `agentctl` directly:

```bash
agentctl health
agentctl tools
agentctl call git.status
agentctl trace -f
```

Set `AGENTD_URL` or pass `--url` if the daemon is running somewhere else.

## Writing a Tool

Tools register named actions. Each action receives a `ctx` handle for host
capabilities. Those capabilities range from shell and filesystem access to
network calls, secrets, memory and model calls. Access is checked against
`grants.toml` and runtime policy before the capability is used.

```lua
agentd.tool({
  name = "git",
  requires = { "shell.exec" },
})

agentd.action({
  name = "git.diff",
  requires = { "shell.exec" },
  handler = function(args, ctx)
    local res = ctx.shell("git", { "-C", args.cwd or ".", "diff" })
    return {
      diff = res.stdout,
      exit_code = res.exit_code,
    }
  end,
})
```

`init.lua` is the entry point. It can load other files with `import(...)`, which
resolves paths relative to the entry file and rejects absolute paths and `..`
traversal.

The `examples/` directory includes a complete setup with a Git tool, runner,
skills, grants and a Discord service example.

## Capability Surface

Actions and services use `ctx` to ask the host to do work. The runtime checks
the corresponding grant before performing the operation.

| API | Purpose | Permission |
| --- | --- | --- |
| `ctx.log.{trace,debug,info,warn,error}` | Structured logging | None |
| `ctx.shell(bin, args, opts?)` | Run a process without a shell | `shell.exec[:<bin>]` |
| `ctx.fs.{read,write,append,exists,stat,list_dir,remove}` | Filesystem access | `fs.read:<path>` / `fs.write:<path>` |
| `ctx.http.{get,post,request,client}` | HTTP requests | `net:<host>` |
| `ctx.ws.connect(url)` | WebSocket client | `net:<host>` |
| `ctx.secret.{get,set,delete,exists,list}` | Credential storage | `secret:<key>` |
| `ctx.memory.create(ns)` | Durable namespaced storage | `memory.read` / `memory.write` |
| `ctx.ai.{ask,complete,providers}` | Model provider calls | `ai:<provider>` |
| `ctx.call(name, args)` | Invoke another action | Target action requirements |
| `ctx.run(name, prompt)` | Invoke a runner | Runner allowlist |
| `ctx.caller` | Invocation identity | None |

The script environment also exposes sandboxed globals for imports, async work,
timers, channels and JSON.

## Permissions

Permissions answer two separate questions:

- What host capabilities does a tool need?
- Which callers are allowed to trigger that tool?

Every action call is evaluated through a default-deny engine:

```text
tool grants
  ∩ action requirements
  ∩ runner allowlist
  ∩ interface allowlist
  ∩ service allowlist
  ∩ policy
```

Component manifests declare requirements, but they do not grant access. Grants
are configured in `grants.toml`.

```toml
[tool.git]
granted = ["shell.exec:git"]

[runner.backend_reviewer]
allowed_actions = ["git.diff", "git.status"]

[policy]
deny_actions = ["shell.exec"]
```

Permission slugs use `domain[:specifier]` syntax:

- `shell.exec:git` allows running the `git` binary.
- `net:api.example.com` allows network access to that host.
- `fs.read:/workspace/**` allows reads below `/workspace`.
- `fs.write:/tmp/agentd/**` allows writes below `/tmp/agentd`.
- `ai:anthropic` allows calls through the Anthropic provider.

Filesystem paths are resolved before permission checks, including symlinks and
`..` segments, so path-scoped grants cannot be bypassed through aliases.

## Interactive Approvals

Some calls should not be silently allowed or denied. When a grant is missing or
an action is marked for confirmation, the runtime can ask an operator for a
decision over the privileged `/control` plane.

Start the approval console:

```bash
agentctl grants listen
```

For each request, the console shows which action was requested, who called it
and which permissions are missing. The operator can choose:

- `once` - allow the current call only.
- `forever` - persist the grant to `grants.toml` and reload permissions.
- `deny` - reject the call.

If no operator is connected before the approval timeout, the request is denied.
Policy denials and allowlist denials are hard denials and are not escalated.

## Runners and Providers

A runner is an AI worker with a model, skills and an action allowlist. Clients
can ask a runner to handle a prompt. The runner can only call actions that its
allowlist permits.

Runners select models with a `"<provider>/<model_id>"` string. The prefix routes
the request through the configured backend.

Built-in provider prefixes:

| Prefix | Backend |
| --- | --- |
| `anthropic` | Anthropic Messages API using the secret store |
| `anthropic-cli` | Local `claude` CLI |
| `openai` | OpenAI-compatible Messages API |
| `codex` | `codex app-server` over JSON-RPC |
| `openai-cli` | Local `codex` CLI text fallback |

If a runner omits the provider prefix, `anthropic` is used by default.

## Configuration

The daemon resolves configuration in this order:

1. CLI flags.
2. Environment variables.
3. `$XDG_CONFIG_HOME/agentd/config.toml`.
4. Built-in defaults.

Useful environment variables:

| Variable | Description |
| --- | --- |
| `AGENTD_CONFIG` | Path to `config.toml`. |
| `AGENTD_INIT` | Path to the entry file. |
| `AGENTD_GRANTS_FILE` | Path to `grants.toml`. |
| `AGENTD_ADDR` | Daemon bind address. |
| `AGENTD_TRACE_FILE` | JSONL trace output path. |
| `AGENTD_LOG` | Runtime log filter. |
| `AGENTD_TOKEN` | Bearer token for `/ws`. |
| `AGENTD_ADMIN_TOKEN` | Bearer token for `/control`. |
| `AGENTD_NO_AUTH` | Disable WebSocket and control-plane auth. |
| `AGENTD_APPROVAL_TIMEOUT_MS` | Approval timeout in milliseconds. |

By default, the daemon creates local bearer tokens automatically:

- Public WebSocket token: `$XDG_STATE_HOME/agentd/token`.
- Control-plane token: `$XDG_STATE_HOME/agentd/admin-token`.

`agentctl` reads these token files automatically for local use. `/health` does
not require authentication.

See `examples/config.toml` for a commented configuration file.

## agentctl

Common console commands:

```bash
agentctl health
agentctl tools
agentctl call <action> [-d key=value | --json '{"key":"value"}']
agentctl runner ls
agentctl runner inspect <name>
agentctl runner run <name> <prompt>
agentctl skills ls
agentctl skills inspect <name>
agentctl services ls
agentctl trace [-f | -n 100]
agentctl grants listen
agentctl packages ls
agentctl packages install <git-url> [--ref <ref>]
agentctl packages update <name>
agentctl packages remove <name>
```

## Packages

Packages bundle reusable components with their requested permissions. Installed
packages live under:

```text
$XDG_DATA_HOME/agentd/packages
```

Installing a package does not automatically trust it. To approve the permission
set declared by a package, add a package trust entry to `grants.toml`:

```toml
[package.example]
trusted = true
```

Explicit tool, runner or service grant entries can still be used to narrow a
package's inherited grants.

## Development

Run the standard checks:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The default test suite is designed to avoid network calls and live model
requests. Optional integration tests are enabled with environment variables:

| Variable | Description |
| --- | --- |
| `AGENTD_TEST_CLAUDE=1` | Run live `claude` CLI provider tests. |
| `AGENTD_TEST_CODEX=1` | Run live `codex app-server` provider and MCP tests. |
| `AGENTD_TEST_KEYRING=1` | Run real OS-keyring secret-store tests. |

## License

MIT.

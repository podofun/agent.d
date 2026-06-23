# ctx — Capability Handle

`ctx` is the per-invocation capability handle passed as the second argument to action handlers and as the first argument to service bodies. Every I/O operation goes through `ctx`; the runtime enforces the [permission engine](/v0/concepts/permissions) on each call.

## Namespaces

| Namespace | What it does | Required permission |
|---|---|---|
| [`ctx.log`](/v0/reference/ctx/logging) | Structured log/trace output | none |
| [`ctx.shell`](/v0/reference/ctx/shell) | Run child processes (argv-only) | `shell.exec[:<bin>]` |
| [`ctx.fs`](/v0/reference/ctx/fs) | Read, write, and inspect files | `fs.read:<path>` / `fs.write:<path>` |
| [`ctx.http`](/v0/reference/ctx/http) | HTTP requests and persistent clients | `net:<host>` |
| [`ctx.ws`](/v0/reference/ctx/websocket) | WebSocket connections | `net:<host>` |
| [`ctx.mailer`](/v0/reference/ctx/mailer) | Send email over SMTP | `net:<host>` |
| [`ctx.secret`](/v0/reference/ctx/secrets) | OS keyring get/set/delete | `secret:<key>` |
| [`ctx.memory`](/v0/reference/ctx/memory) | Durable namespaced key/value (redb) | `memory.read:<ns>` / `memory.write:<ns>` |
| [`ctx.state`](/v0/reference/ctx/memory) | Ephemeral in-process key/value | none |
| [`ctx.ai`](/v0/reference/ctx/ai) | Model calls through registered providers | `ai:<provider>` |
| [`ctx.call` / `ctx.run` / `ctx.structured`](/v0/reference/ctx/calls) | Cross-component action and runner invocation | depends on target |
| [`ctx.caller`](/v0/reference/ctx/caller) | Read-only caller identity | none |

## Helper globals

Several capabilities are exposed as **globals** rather than `ctx.*` methods because they are general-purpose coroutine and data utilities, not gated by the permission engine:

| Global | Page |
|---|---|
| `sleep`, `async`/`await`, `parallel`, `parallel_map`, `channel`, `timer` | [Concurrency](/v0/reference/ctx/concurrency) |
| `json`, `string.*` helpers | [Standard library](/v0/reference/ctx/stdlib) |

See [Writing context](/v0/writing/context) for a narrative guide on using `ctx`.

## See also

- [Concepts: permissions](/v0/concepts/permissions)
- [Writing context](/v0/writing/context)
- [Security: permission slugs](/v0/security/permission-slugs)
- [Security: grants](/v0/security/grants)

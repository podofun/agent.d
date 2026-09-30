# How it works

This page follows one request from the moment you ask for it to the moment the result comes back. It is the mental model the rest of the docs build on, so it is worth reading once before you write your first tool.

## Three files and a background service

A setup comes down to three files in a config folder:

- **`init.lua`** describes what exists: the tools and their actions, the agents (called runners) with their models and instructions, and any background services such as a bot or a poller.
- **`grants.toml`** decides what each of those is allowed to do. It is the only place access is ever given.
- **`config.toml`** holds runtime settings such as the address to listen on and which model providers are available. Most setups barely touch it.

When you start `agentd`, it reads `init.lua` once, registers everything it describes, and starts listening, by default on `127.0.0.1:7777`. From then on every client shares that one loaded setup: the terminal chat, `agentctl`, your own application, and the services running inside the runtime. Nothing is loaded per request and nothing is copied per client.

While you are building, start it with `--watch`. It then reloads as soon as you save `init.lua`, anything it imports, a skill file, or `grants.toml`. Requests already running finish on the old version, and stored memory and a connected approver carry over.

## The life of a request

Say you open the chat and ask the code reviewer to look at your staged changes. The runner decides it needs the diff and asks for the `git.diff` action. Here is what happens next.

**The runtime finds who owns the action.** Every action belongs to a tool, so `git.diff` belongs to the `git` tool. An action nobody registered fails straight away.

**Every layer has to say yes.** The runtime checks the call against the tool's grants, what the action declares it needs, what this runner is allowed to call, what the connecting client is allowed to call, and any global policy. Access is the overlap of all of them, so a single missing yes means no. The [permission model](/v0/concepts/permissions) walks through each layer.

**A missing grant can become a question instead of an error.** If an approver is connected, for example you in the chat, the runtime pauses and asks. You can allow it once, allow it from now on (which writes the grant into `grants.toml` for you), or deny it. Nobody answering within two minutes counts as a no. Rules you mark as hard denials are never offered for approval. See [Interactive approvals](/v0/security/approvals).

**The handler runs with only what it was granted.** Your Lua handler receives the call's arguments and a `ctx` handle, which is how it reaches the outside world: running programs, reading files, making HTTP requests, reading secrets, storing memory, calling a model, or calling another action. Each of those is checked again at the moment it is used, and any program it starts runs inside the [shell sandbox](/v0/security/sandbox), confined to the folders and hosts you granted.

**The result goes back and the call is recorded.** Whatever the handler returns is sent back to the caller, here the runner, which reads the diff and writes its review. Every call also lands in a trace log you can follow live with `agentctl trace -f`, which is the first place to look when an agent does something you did not expect. See [Observability](/v0/operations/observability).

## Who is asking

Every call carries the identity of whoever made it: the client that connected, the user it is acting for, the runner that asked, or the background service that is running. Grants can be written for any of them, so a Discord bot and your own terminal can share the same tools with different permissions. See [Interfaces and callers](/v0/concepts/interfaces-and-callers).

## Where to go next

- [Quick start](/v0/guide/quick-start) puts this into practice with the bundled example.
- [Concepts](/v0/concepts/) describes each building block in more depth.
- [Protocol reference](/v0/reference/protocol) documents the WebSocket API, for when you connect your own application.

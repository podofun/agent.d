# How it works

This page follows one request from the moment you ask for it to the moment the result comes back. The rest of the docs build on this picture, so read it once before you write your first tool.

## Three files and a background service

A setup comes down to three files in a config folder:

- **`init.lua`** describes what exists: the tools and their actions, the agents (called runners) with their models and instructions, and any background services such as a bot or a poller.
- **`grants.toml`** decides what each of those is allowed to do. All access is given here.
- **`config.toml`** holds runtime settings such as the address to listen on and which model providers are available. Most setups change little here.

When you start `agentd`, it reads `init.lua` once, registers everything it describes, and starts listening, by default on `127.0.0.1:7777`. From then on every client shares that one loaded setup: the terminal chat, `agentctl`, your own application, and the services running inside the runtime.

While you are building, start it with `--watch`. It then reloads as soon as you save `init.lua`, anything it imports, a skill file, or `grants.toml`. Requests already running finish on the old version, and stored memory and a connected approver carry over.

## The life of a request

Say you open the chat and ask the code reviewer to look at your staged changes. The runner decides it needs the diff and asks for the `git.diff` action. Here is what happens next.

**The runtime finds who owns the action.** Every action belongs to a tool, so `git.diff` belongs to the `git` tool. A call to an action nobody registered fails straight away.

**The runtime checks denials, then grants.** Denials are things nobody may do, whatever the grants say. After them, the runtime checks the call against what this runner may call, the tool's grants, and what the action declares it needs. Every check has to pass. The [permission model](/v0/concepts/permissions) walks through each one.

**A missing grant becomes a question.** The runtime pauses the call and asks. You can allow it once, allow it from now on (which writes the grant into `grants.toml` for you), or deny it. If nobody answers within two minutes, the call is refused. You choose who answers for each runner: you, a classifier runner that checks each step, or nobody, for bots that should run on their grants alone. A denied call is refused without a question. See [Interactive approvals](/v0/security/approvals).

**The handler runs with what it was granted.** Your Lua handler receives the call's arguments and a `ctx` handle, which is how it reaches the outside world: running programs, reading files, making HTTP requests, reading secrets, storing memory, calling a model, or calling another action. Each of those is checked again at the moment it is used. Any program the handler starts runs inside the [shell sandbox](/v0/security/sandbox), which limits it to the folders and hosts you granted.

**The result goes back and the call is recorded.** Whatever the handler returns goes back to the caller, here the runner, which reads the diff and writes its review. Every call is also written to a trace log you can follow live with `agentctl trace -f`. Look there first when an agent does something you did not expect. See [Observability](/v0/operations/observability).

## Who is asking

Every call carries the identity of whoever made it: the client that connected, the user it is acting for, the runner that asked, or the background service that is running. Grants can be written for any of them, so a Discord bot and your own terminal can share the same tools with different permissions. See [Interfaces and callers](/v0/concepts/interfaces-and-callers).

## Where to go next

- [Quick start](/v0/guide/quick-start) puts this into practice with the bundled example.
- [Concepts](/v0/concepts/) describes each building block in more depth.
- [Protocol reference](/v0/reference/protocol) documents the WebSocket API, for when you connect your own application.

# What is agent.d?

agent.d is a runtime for AI agents that do real work on a machine. You describe what an agent can do, which model it talks to, and what it is allowed to touch. It runs the agent as a small background service and gives you a terminal chat, a command line, and a WebSocket API to reach it from anywhere else.

It does not care what the agent is for. The same runtime can host a code reviewer that reads your Git diff, a support bot on Discord, a job that watches a webhook and files tickets, or a personal assistant that remembers your preferences across weeks.

## Why run agents this way

Getting a model to call a tool takes an afternoon. Trusting it with your machine is the hard part. You need to know which programs it can run, which files it can read, which hosts it can reach, and what happened while you were not looking. You also want to reuse the agent from more than one place without copying its tools into each one.

This runtime is built around those questions.

**Nothing runs unless you allow it.** Every capability starts switched off: running a program, reading a file, reaching a host, reading a secret, calling a model. You switch them on in one file, `grants.toml`, kept apart from the code that uses them, so a tool can say what it needs but can never give itself access. When an agent asks for something you have not granted, you can approve it once or for good from the chat, instead of editing config and restarting.

**Allowed commands still run in a sandbox.** Granting an agent `git` does not hand it your home directory. Every program it starts is confined to the folders and hosts you granted, on Linux, macOS, and Windows, and that confinement follows any program the command starts in turn.

**Write a capability once, use it everywhere.** Tools, agents, and background services are short Lua files. Once the runtime loads them, the same agent answers in the terminal chat, from `agentctl`, from your own app, and from a bot running as a service, with the same permissions each time.

**Pick any model and change your mind later.** An agent names its model as one string, such as `anthropic/claude-opus-4-7`, `openai/gpt-4.1`, or `ollama/qwen3:14b` for a model running on your own machine. Hosted APIs, local servers, Claude Code, and Codex all plug in the same way, so switching providers never means rewriting your tools.

## What you get

Installing it gives you two programs. `agentd` is the runtime itself: it loads your Lua files, enforces your grants, and keeps running in the background. `agentctl` is how you talk to it, whether that is opening a chat with an agent, calling a single action, approving a request, or following the activity log.

## Next steps

- [Installation](/v0/guide/installation) gets both programs onto your machine.
- [Quick start](/v0/guide/quick-start) has a working agent answering you in a few minutes.
- [How it works](/v0/guide/how-it-works) explains what happens between your request and the result.

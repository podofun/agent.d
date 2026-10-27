# What is agent.d?

agent.d is a runtime for AI agents that do real work on a machine. You describe what an agent can do, which model it talks to, and what it is allowed to touch. It runs the agent as a small background service and gives you a terminal chat, a command line, and a WebSocket API to reach it from anywhere else.

The agent can be for anything. The same runtime can host a code reviewer that reads your Git diff, a support bot on Discord, a job that watches a webhook and files tickets, or a personal assistant that remembers your preferences across weeks.

## Why run agents this way

Getting a model to call a tool takes an afternoon. Trusting it with your machine is the hard part. You need to know which programs it can run, which files it can read, which hosts it can reach, and what happened while you were not looking. You also want to reuse the agent from more than one place without copying its tools into each one.

agent.d is built around those questions.

### You choose what runs

Every capability starts switched off: running a program, reading a file, reaching a host, reading a secret, calling a model. You switch them on in one file, `grants.toml`, kept apart from the code that uses them. A tool says what it needs, and you decide what it gets. When an agent asks for something you have not granted, you can approve it once or for good from the chat, without editing config or restarting.

### Commands run in a sandbox

Granting an agent `git` does not hand it your home directory. Every program it starts is limited to the folders and hosts you granted, on Linux, macOS, and Windows. The same limits apply to any program that command starts in turn.

### One setup for every client

Tools, agents, and background services are short Lua files. Once the runtime loads them, the same agent answers in the terminal chat, from `agentctl`, from your own app, and from a bot running as a service, with the same permissions each time.

### Any model

An agent names its model as one string, such as `anthropic/claude-opus-4-7`, `openai/gpt-4.1`, or `ollama/qwen3:14b` for a model running on your own machine. Hosted APIs, local servers, Claude Code, and Codex all plug in the same way. To switch providers, you change that string, and your tools stay as they are.

## What you get

Installing it gives you two programs. `agentd` is the runtime itself: it loads your Lua files, enforces your grants, and keeps running in the background. `agentctl` is how you talk to it: you can open a chat with an agent, call a single action, approve a request, or follow the activity log.

## Next steps

- [Installation](/v0/guide/installation) gets both programs onto your machine.
- [Quick start](/v0/guide/quick-start) has a working agent answering you in a few minutes.
- [How it works](/v0/guide/how-it-works) explains what happens between your request and the result.

# Quick start

In the next few minutes you will start the runtime with a small example setup, call one of its tools yourself, and then hand the same tool to an AI agent and ask it to review your changes. The example is a code reviewer that can look at your Git status and diff, and nothing else.

You need the runtime installed (see [Installation](/v0/guide/installation)) and Git. For the last step you also need a model to talk to. Any of these will do: an Anthropic or OpenAI API key, or Claude Code or Codex already signed in on your machine.

## Get the example

The example lives in the project repository, so clone it and work from there:

```bash
git clone https://github.com/podofun/agent.d
cd agent.d
```

The setup is four short files under `examples/`. `init.lua` loads a `git` tool with a `git.status` and a `git.diff` action, a couple of instruction files called skills, and one agent, `backend_reviewer`. `grants.toml` allows the `git` tool to run the `git` program, and allows the reviewer to use one model provider and those two actions. Nothing else is permitted.

## Start the runtime

```bash
agentd --watch --init examples/init.lua --grants examples/grants.toml
```

It prints the address it is listening on, `http://127.0.0.1:7777`, and how many actions, runners, and skills it loaded. `--watch` makes it reload by itself whenever you save one of the example files, so you never need to restart it while following along. Leave it running and open a second terminal in the same folder for the rest of this guide.

## Call a tool yourself

Before involving a model, check what the runtime loaded and run one action by hand:

```bash
agentctl tools
agentctl call git.status --result-only
```

The first command lists `git.diff` and `git.status`. The second runs `git status` in the folder you started the runtime from and prints the result as JSON. This is the same call an agent makes, going through the same permission checks. You did not need an API key for it.

## Pick a model

The reviewer does not care which model it runs on. Use whichever one you already have: two lines decide it, the `model` line in `examples/runners/backend_reviewer.lua` and the `ai:` grant under `[runner.backend_reviewer]` in `examples/grants.toml`. The example comes set up for Anthropic.

| What you have | `model` line | Grant | One-time setup |
|---|---|---|---|
| An Anthropic API key | `anthropic/claude-opus-4-7` | `ai:anthropic` | Store the key, see below |
| An OpenAI API key | `openai/gpt-4.1` | `ai:openai` | Store the key, see below |
| Claude Code, signed in | `anthropic-cli/sonnet` | `ai:anthropic-cli` | None, it uses your Claude login |
| Codex, signed in | `codex/` | `ai:codex` | None, it uses your Codex login |

For example, to run the reviewer on Claude Code instead, the two lines become:

```lua
model = "anthropic-cli/sonnet",
```

```toml
granted = ["ai:anthropic-cli"]
```

If you use an API key, store it in your system keychain. Piping it in keeps it out of your shell history. Use `openai_api_key` in place of `anthropic_api_key` for OpenAI:

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```

Saving either file is enough, the runtime picks up the change on its own. To use a model running on your own machine, such as one served by Ollama, see [Custom providers](/v0/providers/custom); the model needs to support tool calling to use the reviewer's actions. [Providers](/v0/providers/) covers every option in more detail.

## Talk to the agent

Open a chat with the reviewer:

```bash
agentctl chat --runner backend_reviewer
```

Ask it something like "What have I changed, and is any of it risky?" You will see it call `git.status` and `git.diff`, each shown as a collapsed row you can click to expand, before it writes its answer. Press Ctrl+C to leave the chat.

## See what happened

Every call is recorded. To watch calls as they happen, run this in another terminal and ask the reviewer something else:

```bash
agentctl trace -f
```

Each line is one event, such as an action call, a runner invocation, or a permission decision, with its timing and result. It is the first place to look when an agent does something unexpected.

## What to try next

See the permission system for yourself. Remove `"shell.exec:git"` from the `[tool.git]` grants in `examples/grants.toml` and save. Then open the chat and ask the reviewer about your changes once more.

This time the runtime stops before `git` ever starts, and the chat asks you whether to allow it. Press `o` to allow it once, `f` to allow it from now on (which writes the grant back into `grants.toml` for you), or `d` to refuse and watch the reviewer report that it could not read your changes. Nothing an agent does gets past that question without you.

When you are ready to build your own setup from an empty folder, the [tutorial](/v0/tutorial/) takes you through a tool, its permissions, and an agent that uses it, step by step.

<div align="center">

<img src="assets/agentd_logo.png" alt="agent.d" width="150" height="150" />

# agent.d

**A small runtime for building and operating tool-using AI agents.**

[Documentation](https://docs.podo.fun/agentd/v0/guide/what-is-agentd) · [Quick start](https://docs.podo.fun/agentd/v0/guide/quick-start) · [Recipes](https://docs.podo.fun/agentd/v0/recipes/) · [Releases](https://github.com/podofun/agent.d/releases)

</div>

agent.d runs AI agents as a local service. You define tools, agent behavior, and long-running services in Lua. You define permissions in TOML. The Rust runtime loads those definitions, connects them to model providers, and enforces access to the host.

The main idea is simple: tools should not belong to one chat UI or one model SDK. Define them once, give them explicit permissions, and call them from the CLI, a WebSocket client, an integration, or another agent.

agent.d is useful when an agent needs to do real work on a machine and you need clear answers to questions such as:

- Which programs may it run?
- Which files may it read or change?
- Which hosts may it contact?
- Which tools may a specific agent or client call?
- What happened during the last run?
- Can a person approve one exceptional request without disabling the policy?

## What the runtime provides

- **Tools and actions.** Wrap local programs and APIs in reusable Lua functions that agents and clients can call.
- **Runners.** Combine a model, system instructions, skills, and an allowed set of actions.
- **Skills.** Keep reusable instructions in Markdown and load them into one or more runners.
- **Services.** Run long-lived integrations and event loops in the same controlled runtime.
- **Memory and state.** Store durable namespaced data or share short-lived state between components.
- **Approvals.** Use a human-in-the-loop to allow a denied request once or persist a specific grant.
- **Permissions and sandboxing.** Limit programs, files, network access, secrets, models, and component calls with default-deny grants and native OS confinement.
- **Tracing.** Record action calls, runner activity, permission decisions, results, errors, and timings as JSONL.
- **Provider routing.** Select Anthropic, OpenAI-compatible APIs, Claude CLI, or Codex backends per runner.
- **Hot reload.** Reload Lua modules, skills, and grants during development with `agentd --watch`.

## A practical example

The following defines a small code-review agent. It can read the staged Git diff and send it to a model. It is not granted general shell access or filesystem access.

```lua
-- init.lua
agentd.tool({ name = "git", requires = { "shell.exec:git" } })

agentd.action({
  name = "git.diff",
  requires = { "shell.exec:git" },
  handler = function(args, ctx)
    local result = ctx.shell("git", {
      "-C", args.cwd or ".", "diff", "--staged",
    })
    return { diff = result.stdout, exit_code = result.exit_code }
  end,
})

agentd.runner({
  name = "review",
  model = "anthropic/claude-opus-4-7",
  system = "Review the staged diff. Find correctness and security issues. Cite files and be concise.",
  actions = { "git.diff" },
})
```

The declarations above describe what the components need. They do not grant access. Grants live separately:

```toml
# grants.toml
[tool.git]
granted = ["shell.exec:git"]

[runner.review]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff"]
```

Start the daemon and run the reviewer:

```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
agentd --init init.lua --grants grants.toml
```

```bash
# Run from another terminal, inside a Git repository
agentctl runner run review "Review what I am about to commit" --text-only
```

The runner can call `git.diff`, and that `git.diff` action can execute `git`.

## Install

Download a release for Linux, macOS, or Windows from [GitHub Releases](https://github.com/podofun/agent.d/releases), or build from source with Rust 1.85 or newer:

```bash
git clone https://github.com/podofun/agent.d
cd agent.d
cargo build --release
```

This produces `target/release/agentd` and `target/release/agentctl`. Windows releases also include `agentd-netbroker.exe`; keep it beside `agentd.exe`. Follow the [installation guide](https://docs.podo.fun/agentd/v0/guide/installation) for PATH setup and the one-time Windows sandbox installation.

## Quick start

The bundled examples includes a small Git tool, sample skill files, and a basic code-review runner:

```bash
agentd --init examples/init.lua --grants examples/grants.toml
```

In another terminal:

```bash
agentctl health
agentctl tools
agentctl call git.status --result-only
```

Use `--watch` while editing a project:

```bash
agentd --watch --init examples/init.lua --grants examples/grants.toml
```

The [five-minute quick start](https://docs.podo.fun/agentd/v0/guide/quick-start) explains each command. The [tutorial](https://docs.podo.fun/agentd/v0/tutorial/) starts from scratch and covers the full workflow.

## Documentation

- [What is agent.d?](https://docs.podo.fun/agentd/v0/guide/what-is-agentd)
- [How the runtime works](https://docs.podo.fun/agentd/v0/guide/how-it-works)
- [Tools and actions](https://docs.podo.fun/agentd/v0/concepts/tools-and-actions)
- [Runners](https://docs.podo.fun/agentd/v0/concepts/runners)
- [Permissions](https://docs.podo.fun/agentd/v0/concepts/permissions)
- [Providers](https://docs.podo.fun/agentd/v0/providers/)
- [Recipes](https://docs.podo.fun/agentd/v0/recipes/)
- [`ctx` API reference](https://docs.podo.fun/agentd/v0/reference/ctx/)

## Project status

agent.d is in its alpha stage and remains experimental. It is being developed as the runtime for Podofun's AI features, and its APIs and behavior may change as those features evolve. Bug reports, focused pull requests, and feedback from real workloads are welcome.

## License

[MIT](./LICENSE)

<div align="center">

<img src="assets/agentd_logo.png" alt="agentd" width="160" height="160" />

# agent.d

**A portable runtime for tool-using AI agents.**

[Documentation](https://docs.podo.fun/agentd/v0/guide/what-is-agentd) ·
[Quick start](https://docs.podo.fun/agentd/v0/guide/quick-start) ·
[Tutorial](https://docs.podo.fun/agentd/v0/tutorial/) ·
[Reference](https://docs.podo.fun/agentd/v0/reference/ctx/)

</div>

agentd is a small runtime for building AI agents that need to call tools safely.
You define what an agent can do once — its model, memory, external access and
approval rules — and call it from any client: CLI tools, chat integrations,
webhooks, editors or small servers. Run the daemon once; connect clients to it.

- **Define tools once.** Expose actions like `git.status` or `deploy.preview` and call them from any client.
- **Fail closed by default.** Shell, filesystem, network, secrets and model calls all require an explicit grant.
- **Approve on the fly.** A privileged operator can allow a missing grant once or persist it.
- **Stay portable.** Frontends reuse the same agent definitions instead of carrying their own copies.
- **Multi-provider support.** Anthropic, OpenAI, Claude CLI, Codex, and any other OpenAI-compatible API provider.

## A quick taste

Define a tool in Lua:

```lua
-- init.lua
agentd.tool({ name = "git", requires = { "shell.exec:git" } })

agentd.action({
  name = "git.status",
  requires = { "shell.exec:git" },
  handler = function(args, ctx)
    local res = ctx.shell("git", { "status", "--porcelain=v1" })
    return { status = res.stdout, exit_code = res.exit_code }
  end,
})
```

Grant it the one capability it needs — nothing is granted implicitly:

```toml
# grants.toml
[tool.git]
granted = ["shell.exec:git"]
```

Run it and call the action:

```bash
agentd --init init.lua --grants grants.toml
agentctl call git.status
```

That's the whole loop. Tools, runners (AI workers), skills, services, durable
memory and the permission model are covered in the docs.

## Install

Download a pre-built binary for Linux, macOS or Windows from the
[releases page](https://github.com/podofun/agent.d/releases), or build from source:

```bash
git clone https://github.com/podofun/agent.d
cd agent.d
cargo build --release   # produces target/release/{agentd,agentctl}
```

See [Installation](https://docs.podo.fun/agentd/v0/guide/installation) for details.

## Documentation

Full documentation lives at **[docs.podo.fun/agentd](https://docs.podo.fun/agentd/v0/guide/quick-start)**:

- [Quick start](https://docs.podo.fun/agentd/v0/guide/quick-start) — running in five minutes
- [Tutorial](https://docs.podo.fun/agentd/v0/tutorial/) — build your first agent step by step
- [Core concepts](https://docs.podo.fun/agentd/v0/concepts/) — tools, runners, skills, services, permissions
- [Capability reference](https://docs.podo.fun/agentd/v0/reference/ctx/) — the full `ctx` API
- [Permissions & security](https://docs.podo.fun/agentd/v0/security/grants) — grants and the permission model
- [Recipes](https://docs.podo.fun/agentd/v0/recipes/) — Discord bots, HTTP tools, and more

## License

[MIT](./LICENSE).

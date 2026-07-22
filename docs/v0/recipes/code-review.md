# Recipe: Code review runner

A minimal AI code-review agent: a `git` tool that shells out to the `git` binary, a reusable `reviewer` skill defined in Markdown, and a runner that composes them. You can invoke it from the terminal in seconds with `agentctl runner run`.

This recipe is based directly on the bundled `examples/tools/git.lua`, `examples/runners/backend_reviewer.lua`, and `examples/skills/reviewer.md`.

## Configuration layout

```
my-reviewer/
├── init.lua
├── tools/
│   └── git.lua
├── runners/
│   └── backend_reviewer.lua
├── skills/
│   └── reviewer.md
└── grants.toml
```

## Step 1 — The git tool

**Required permission:** `shell.exec:git`

```lua
-- tools/git.lua
agentd.tool({
  name = "git",
  requires = { "shell.exec:git" },
})

local function git(ctx, args, sub)
  args = args or {}
  local argv = { "-C", args.cwd or "." }
  for _, a in ipairs(sub) do
    table.insert(argv, a)
  end
  local res = ctx.shell("git", argv, { separate_stderr = false })
  return { exit_code = res.exit_code, output = res.stdout }
end

agentd.action({
  name = "git.diff",
  requires = { "shell.exec:git" },
  handler = function(args, ctx)
    args = args or {}
    local sub = { "diff" }
    if args.staged then
      table.insert(sub, "--staged")
    end
    ctx.log.info("git.diff cwd=" .. (args.cwd or "."))
    local r = git(ctx, args, sub)
    return { diff = r.output, exit_code = r.exit_code }
  end,
})

agentd.action({
  name = "git.status",
  requires = { "shell.exec:git" },
  handler = function(args, ctx)
    local r = git(ctx, args, { "status", "--porcelain=v1" })
    return { status = r.output, exit_code = r.exit_code }
  end,
})
```

`ctx.shell(bin, args, opts)` takes an argv array — no shell string, no injection surface. `separate_stderr = false` merges stderr into stdout so the diff output is complete.

## Step 2 — The reviewer skill

Skills are Markdown files with YAML frontmatter. The `name` field must match the string you pass to `skills` in the runner.

```markdown
---
name: reviewer
description: meticulous code reviewer
actions:
  - git.diff
  - git.status
---
You are a meticulous code reviewer. Focus on correctness, clarity, and test
coverage. Cite file paths and line numbers. Prefer concrete suggestions over
hand-waving.
```

Save this as `skills/reviewer.md`.

## Step 3 — The runner

**Required permission:** `ai:<provider>` (here `ai:anthropic`)

```lua
-- runners/backend_reviewer.lua
agentd.runner({
  name = "backend_reviewer",
  system = "Reply in plain text. No markdown headers.",
  model = "anthropic/claude-opus-4-7",
  skills = { "reviewer" },
  actions = { "git.diff", "git.status" },
})
```

`model` is `"<provider>/<model_id>"`. The `actions` list is an advisory allowlist — the grants file is still the authoritative source of truth for what the runner may call.

## Step 4 — The entry point

```lua
-- init.lua
import("tools/git.lua")
import("runners/backend_reviewer.lua")
agentd.skills.load("skills/reviewer.md")
```

`import()` resolves relative to `init.lua`. `agentd.skills.load()` loads a single skill file and returns its name.

## Step 5 — grants.toml

```toml
[tool.git]
granted = ["shell.exec:git"]

[runner.backend_reviewer]
granted = ["ai:anthropic"]
allowed_actions = ["git.diff", "git.status"]
```

`[tool.git]` grants the `shell.exec:git` slug to every action under that tool. `[runner.backend_reviewer]` grants `ai:anthropic` so the runner can make model calls, and restricts which actions the runner may invoke at layer 3 of the permission engine.

::: tip Provider credentials
The `anthropic` provider reads your API key from the secret store. Seed it once with `agentctl secret set`:
```bash
echo "$ANTHROPIC_API_KEY" | agentctl secret set anthropic_api_key
```
See [Providers: credentials](/v0/providers/credentials) for details.
:::

## How to run

```bash [release]
agentd --init my-reviewer/init.lua --grants my-reviewer/grants.toml
```

```bash [cargo]
cargo run -p daemon -- --init my-reviewer/init.lua --grants my-reviewer/grants.toml
```

Confirm the daemon loaded correctly:

```bash
agentctl health
# ok
agentctl runner ls
# backend_reviewer
```

## Invoke the runner

```bash
agentctl runner run backend_reviewer "Review my staged changes. Be terse."
```

Add `--text-only` to strip the metadata envelope and get just the review text.

You can also call the underlying actions directly to test them in isolation:

```bash
agentctl call git.status
agentctl call git.diff -d staged=true
```

## Verify

After `agentctl runner run` returns, you should see:

- A plain-text review citing file paths and line numbers (the `reviewer` skill instructs this).
- No markdown headers (the runner's `system` field overrides).
- Exit with no error; `agentctl trace -n 5` shows the tool calls the runner made.

```bash
agentctl trace -n 10
```

## See also

- [Writing tools](/v0/writing/tools)
- [Writing runners](/v0/writing/runners)
- [Writing skills](/v0/writing/skills)
- [ctx.shell reference](/v0/reference/ctx/shell)

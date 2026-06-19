# Step 1 — Project Layout

You will create a small directory that holds every file the daemon needs: an entry point, a tool, a skill, a runner, and a grants file. This page explains the role of `init.lua` and the `import()` function before you write a single line of logic.

## Create the project directory

```bash
mkdir -p ~/projects/git-reviewer
cd ~/projects/git-reviewer
mkdir -p tools skills runners
```

Your final layout will look like this:

```
git-reviewer/
├── init.lua            ← single entry point
├── grants.toml         ← permission grants
├── tools/
│   └── git.lua
├── skills/
│   └── reviewer.md
└── runners/
    └── backend_reviewer.lua
```

## The entry point: `init.lua`

The daemon evaluates **one** Lua file at startup — the file you pass to `--init`. Everything else is pulled in from that file using `import()`. There is no automatic discovery; nothing runs unless it is reachable from `init.lua`.

`import(path)` resolves paths **relative to the entry file**. It refuses absolute paths and any path containing `..`, so your project stays self-contained.

Here is the minimal `init.lua` you will grow over the next steps:

```lua
-- init.lua
import("tools/git.lua")

agentd.skills.dir("skills")

import("runners/backend_reviewer.lua")
```

Three lines do three things:

1. `import("tools/git.lua")` — loads the git tool and its actions.
2. `agentd.skills.dir("skills")` — scans `skills/` and loads every `*.md` file as a skill.
3. `import("runners/backend_reviewer.lua")` — registers the runner.

Save this file now. You will fill in the other files in the steps that follow.

::: tip One file is fine
For small agents you can put everything in `init.lua` itself — tool, actions, skills, runner. Splitting into subdirectories is a convention, not a requirement.
:::

## Next step

[Step 2 — Write your first tool →](/v0/tutorial/first-tool)

## See also

- [Writing init.lua](/v0/writing/init)
- [Concepts: runtime](/v0/concepts/runtime)
- [Concepts: tools and actions](/v0/concepts/tools-and-actions)

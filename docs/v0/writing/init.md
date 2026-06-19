# init.lua — Project Entry Point

`init.lua` is the single file the daemon evaluates at startup. Everything else in
your project — tools, runners, skills, services — is pulled in from here via
`import()` or the skill loaders.

## How `import` works

```lua
import("tools/git.lua")
import("runners/backend_reviewer.lua")
```

`import` resolves paths **relative to the entry file** (`init.lua`). Two path
forms are rejected with an error at load time:

- Absolute paths (`/home/user/something.lua`)
- Parent-directory traversal (`../shared/util.lua`)

This keeps every project's dependency graph rooted in one place and prevents
components from reaching outside their own tree.

::: tip
`import` de-duplicates paths: calling `import` on the same canonicalized path a second time is a no-op — the file is not re-evaluated. You can safely import shared helpers from multiple files without worrying about double-registration.
:::

## Organizing a project

A flat single file is fine for small setups. For anything larger, a conventional
layout keeps things navigable:

```
~/.config/agentd/
├── init.lua            ← entry point
├── grants.toml
├── tools/
│   ├── git.lua
│   └── files.lua
├── runners/
│   └── reviewer.lua
├── services/
│   └── poller.lua
└── skills/             ← Markdown skill files
    ├── reviewer.md
    └── terse.md
```

## Loading skills

agent.d provides three skill helpers on the `agentd.skills` table:

| Call | Effect |
|---|---|
| `agentd.skills.dir(path)` | Loads every `*.md` file found under `path`. Returns the count loaded. |
| `agentd.skills.load(path)` | Loads a single `.md` skill file. Returns the skill name. |
| `agentd.skills.list()` | Returns a `string[]` of currently registered skill names. |

Paths passed to `dir` and `load` follow the same rules as `import`: relative to
the entry file, no absolute paths, no `..` traversal.

## A realistic init.lua

```lua
-- tools
import("tools/git.lua")
import("tools/files.lua")

-- skills from a directory (loads all *.md files)
agentd.skills.dir("skills")

-- inline skill — alternative to a Markdown file
agentd.skill({
  name = "terse",
  description = "no preamble, no markdown headers",
  system = "Reply in plain text. No preamble. No markdown headers.",
})

-- runners
import("runners/reviewer.lua")

-- services
import("services/poller.lua")
```

The daemon evaluates this file in order. Registration calls (`agentd.tool`,
`agentd.action`, `agentd.runner`, `agentd.skill`, `agentd.service`) can appear in
any imported file; the runtime collects them all before serving requests.

## Hot reload

When the daemon runs with `--watch` (or `AGENTD_WATCH=true`), it watches
`init.lua`, every file pulled in via `import()`, loaded skill `.md` sources, and
`grants.toml`. Any change triggers a full rebuild of the runtime in place —
in-flight requests drain on the old runtime while the new one takes over. Durable
[memory](/v0/concepts/memory-and-state) and a connected approval operator survive
reloads.

::: info
`--watch` is a development convenience. Production deployments typically restart
the daemon process when you want to pick up changes.
:::

## See also

- [Project layout](/v0/tutorial/project-layout)
- [Writing tools](/v0/writing/tools)
- [Writing skills](/v0/writing/skills)
- [Writing services](/v0/writing/services)

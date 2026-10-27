# init.lua — Configuration Entry File

`init.lua` is the single file the daemon evaluates at startup. Everything else in
your configuration — tools, runners, skills, services — is pulled in from here via
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

This keeps all configuration dependencies rooted in one place and prevents
components from reaching outside their own tree.

::: tip
`import` de-duplicates paths: calling `import` on the same canonicalized path a second time is a no-op — the file is not re-evaluated. You can safely import shared helpers from multiple files without worrying about double-registration.
:::

## Organizing the configuration directory

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
| `agentd.skills.dir(path)` | Loads every `*.md` file found under `path`. Returns a list of the skills it loaded. |
| `agentd.skills.load(path)` | Loads a single `.md` skill file. Returns the skill. |
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

## Changing what you registered

Every registration call gives back the thing it registered: `agentd.runner{...}` returns the runner, `agentd.action(...)` the action, and the same goes for `agentd.tool`, `agentd.skill`, `agentd.service`, `agentd.skills.load` and `agentd.skills.dir`. What you get back is live. Reading a property shows its current value, and assigning one changes the registered item, so there is nothing else to call:

```lua
local coder = agentd.runner({ name = "coder", model = "anthropic/claude-sonnet-5-5" })

coder.model = "openai/gpt-5"           -- use another model
table.insert(coder.skills, "terse")    -- lists are live too
coder.name = "pair"                    -- renames it; `coder` still points at it
```

This is most useful for changing defaults you did not write. If you import a set of runners and actions from somewhere else, you can adjust them from your own file, imported last, instead of copying and editing them. That way updating the imported files never overwrites your changes.

Each collection on `agentd` can look items up, search for them, and remove them:

| Call | Returns |
|---|---|
| `agentd.runners.get(name)` | The runner, or `nil` if there is none by that name. |
| `agentd.runners.find(query)` | A list of matching runners, sorted by name. |
| `agentd.runners.remove(name)` | Nothing. The runner is unregistered. |

The same three calls exist on `agentd.actions`, `agentd.tools`, `agentd.skills` and `agentd.services`, next to the `list()` each already had. Anything you get back also has `remove()`, so `agentd.runners.get("coder"):remove()` works too.

A query is either a function that receives each item and returns `true` to keep it, or a table of properties that must all match. In a table, a string can use `*` as a wildcard, and a string matches a list property when any item in the list matches:

```lua
-- every git action
for _, action in ipairs(agentd.actions.find({ name = "git.*" })) do
  action.confirm = true
end

-- every runner that uses the terse skill, whatever its model
local terse = agentd.runners.find({ skills = "terse" })

-- anything a function can decide
local slow = agentd.runners.find(function(runner)
  return runner.model == "openai/gpt-5"
end)
```

A few rules keep this predictable:

- An assignment is checked the same way registration checks it, on the line that makes it. A property the item does not have (a typo such as `coder.modle`) or a value of the wrong type is an error, and the item stays as it was.
- An item that others still use cannot be renamed or removed. Removing an action that a runner lists, for example, fails with a message naming the runner, so update the runner first.
- If a later registration replaces an item, or the item is removed, using an older reference to it fails with a message saying so. It never quietly reads the new item.
- Code inside a [package](/v0/packages/) can only change, rename or remove what that package registered, and the names it assigns are scoped under the package the same way its registrations are.
- These calls are meant for `init.lua` and the files it imports, while your configuration loads.

Action properties are `name`, `tool`, `requires`, `confirm`, `cwd`, `input`, `output` and `handler`. Reading `input` or `output` returns the compiled JSON Schema rather than the shorthand you wrote. Runners also accept `actions` as another name for `allowed_actions`.

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

- [Configuration directory](/v0/tutorial/config-directory)
- [Writing tools](/v0/writing/tools)
- [Writing skills](/v0/writing/skills)
- [Writing services](/v0/writing/services)

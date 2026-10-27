# init.lua: the configuration entry file

`init.lua` is the file the daemon runs at startup. The rest of your configuration (tools, runners, skills and services) is pulled in from here with `import()` or the skill loaders.

## How `import` works

Use `import` to split your configuration into several files and load each one from `init.lua`:

```lua
import("tools/git.lua")
import("runners/backend_reviewer.lua")
```

`import` resolves paths **relative to the entry file** (`init.lua`). Two path forms fail with an error at load time:

- Absolute paths (`/home/user/something.lua`)
- Parent-directory traversal (`../shared/util.lua`)

This keeps every file your configuration depends on inside one folder.

::: tip
`import` loads each file once. A second `import` of the same file does nothing, so several files can import a shared helper without registering anything twice.
:::

## Organizing the configuration directory

A single file works well for small setups. For anything larger, this layout keeps things easy to find:

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

Paths passed to `dir` and `load` follow the same rules as `import`: they are relative to the entry file, and absolute paths and `..` are errors.

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

The daemon runs this file from top to bottom. Registration calls (`agentd.tool`, `agentd.action`, `agentd.runner`, `agentd.skill`, `agentd.service`) can appear in any imported file. The daemon collects them all before it serves requests.

## Changing what you registered

Sometimes you want to adjust something after it is registered, for example a runner or action that came from a file you did not write. Every registration call returns what it registered: `agentd.runner{...}` returns the runner, `agentd.action(...)` the action, and the same goes for `agentd.tool`, `agentd.skill`, `agentd.service`, `agentd.skills.load` and `agentd.skills.dir`.

The returned value is a live handle. Reading a property gives its current value, and assigning a property changes the registered item directly:

```lua
local coder = agentd.runner({ name = "coder", model = "anthropic/claude-sonnet-5-5" })

coder.model = "openai/gpt-5"           -- use another model
table.insert(coder.skills, "terse")    -- lists are live too
coder.name = "pair"                    -- renames it; `coder` still points at it
```

This is most useful for defaults you did not write. If you import a set of runners and actions from somewhere else, adjust them from your own file and import that file last. Your changes stay in your file, and updating the imported files leaves them in place.

Each collection on `agentd` can look up an item by name, search for items, and remove them:

| Call | Returns |
|---|---|
| `agentd.runners.get(name)` | The runner, or `nil` if there is none by that name. |
| `agentd.runners.find(query)` | A list of matching runners, sorted by name. |
| `agentd.runners.remove(name)` | Nothing. The runner is unregistered. |

The same three calls exist on `agentd.actions`, `agentd.tools`, `agentd.skills` and `agentd.services`, next to their `list()`. Each handle also has `remove()`, so `agentd.runners.get("coder"):remove()` works too.

A query is either a function or a table. A function receives each item and returns `true` to keep it. A table lists properties that must all match. In a table, a string can use `*` as a wildcard, and a string matches a list property when any item in the list matches:

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

These rules apply to handles:

- An assignment is checked the same way registration checks it, on the line that makes it. A property the item does not have (a typo such as `coder.modle`) or a value of the wrong type is an error, and the item stays as it was.
- An item that something else still uses cannot be renamed or removed. For example, removing an action that a runner lists fails with a message naming the runner. Update the runner first.
- If a later registration replaces an item, or the item is removed, using an older handle to it fails with an error that says so.
- Code inside a [package](/v0/packages/) can change, rename or remove only what that package registered. Names it assigns are scoped under the package, like its registrations.
- Use these calls in `init.lua` and the files it imports, while your configuration loads.

Action properties are `name`, `tool`, `requires`, `confirm`, `cwd`, `input`, `output` and `handler`. Reading `input` or `output` returns the compiled JSON Schema, which can differ from the shorthand you wrote. Runners also accept `actions` as another name for `allowed_actions`.

## Hot reload

When the daemon runs with `--watch` (or `AGENTD_WATCH=true`), it watches `init.lua`, every file pulled in with `import()`, loaded skill `.md` files, and `grants.toml`. Any change rebuilds the configuration in place. Requests already in progress finish on the old configuration while new ones use the new one. Durable [memory](/v0/concepts/memory-and-state) and a connected approver stay in place across reloads.

::: info
`--watch` is meant for development. In production, restart the daemon to pick up changes.
:::

## See also

- [Configuration directory](/v0/tutorial/config-directory)
- [Writing tools](/v0/writing/tools)
- [Writing skills](/v0/writing/skills)
- [Writing services](/v0/writing/services)

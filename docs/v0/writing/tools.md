# Writing Tools and Actions

Tools and actions are how you give agents and clients something to do. A **tool** is a named group, such as `git`, and **actions** are the operations inside it, such as `git.diff`. This page covers `agentd.tool` and `agentd.action`, the handler signature, permission declarations, and the `confirm` flag, with a `git` tool as the running example.

## Registering a tool

Start by registering the tool:

```lua
agentd.tool({
  name    = "git",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
})
```

`requires` on a tool **declares** the permissions that the tool's actions need as a group. The grants themselves live in [grants.toml](/v0/security/grants). You can also declare `requires` on each action, or on both the tool and its actions, and agent.d checks the combined list.

## Registering actions

### Full form

```lua
agentd.action({
  name    = "git.diff",                  -- fully-qualified "tool.action"
  requires = { "shell.exec:git" },       -- permission: shell.exec:git
  confirm = false,                       -- set true to require interactive approval
  tool    = "git",                       -- optional: inferred from name if omitted
  handler = function(args, ctx)
    -- args: table of caller-supplied arguments
    -- ctx:  per-invocation capability handle
    local res = ctx.shell("git", { "diff" })
    return { diff = res.stdout, exit_code = res.exit_code }
  end,
})
```

### Short form

When an action needs only a name and a handler, use the two-argument short form:

```lua
agentd.action("git.ping", function(args, ctx)
  return { ok = true }
end)
```

## The handler signature

```lua
function(args, ctx) -> JSON-serializable table | nil
```

| Parameter | Type | Description |
|---|---|---|
| `args` | `table \| nil` | The arguments the caller sent. Check for `nil` before you index it. |
| `ctx` | `table` | The capability handle for this call. See [ctx overview](/v0/writing/context). |

The return value must be a Lua table that can be turned into JSON, or `nil`. Returning anything else, such as a function or userdata, is a runtime error.

## Describing arguments and results

By default a handler receives whatever arguments the caller sends, and the handler has to check them. To let agent.d check them for you, describe the shape of an action's arguments with an `input` table, and the shape of its result with an `output` table. Both are optional.

```lua
agentd.action({
  name     = "github.create_issue",
  requires = { "net:api.github.com", "secret:GITHUB_TOKEN" },

  input = {
    repo      = { type = "string", desc = "owner/name", required = true },
    title     = { type = "string", min_len = 1, required = true },
    body      = { type = "string" },
    labels    = { type = "array", items = "string" },
    assignees = { type = "array", items = "string", max_items = 10 },
  },

  output = {
    number = { type = "integer" },
    url    = { type = "string" },
  },

  handler = function(args, ctx)
    -- args.repo and args.title are guaranteed to be present and of the right
    -- type here, so no defensive checks are needed.
    ...
  end,
})
```

Describing the arguments helps in two ways:

- **Models call the action correctly.** When a runner offers the action to a model as a tool, the model sees the argument shape and fills in the right field names and types. Without an `input` table, the model knows only that the action takes "some object" and has to guess.
- **Bad calls are rejected early.** Arguments are checked against `input` before the handler runs, and the return value is checked against `output` after. If a check fails, the call returns a validation error that points at the field (for example, `/assignees: array exceeds maxItems 10`), and the handler does not run.

::: tip Reuse the output schema for model replies
If your handler asks a model to produce the action's result, pass `validate = "inherit"` to `ctx.structured`. The reply is checked against the action's own `output` schema, and the model is asked again if it does not match. You write the shape once, in `output`. See [ctx.structured](/v0/reference/ctx/calls#validate-inherit).
:::

### Writing a schema

Each entry in an `input` (or `output`) table is one field. The key is the field name, and the value describes it:

```lua
input = {
  repo  = { type = "string", required = true },
  body  = { type = "string" },
}
```

Every field needs a `type`. The supported types and their options are:

| `type` | Options | Notes |
|---|---|---|
| `"string"` | `min_len` | Minimum length. |
| `"integer"` | None | Whole numbers only. |
| `"number"` | None | Any number. |
| `"boolean"` | None | |
| `"array"` | `items`, `max_items` | `items` is the element type. |
| `"object"` | `props` | `props` is a nested table of fields. |

Every field also accepts:

- `desc`: a description for people and the model, which sees it.
- `required`: whether the field must be present. **Fields are optional by default.** Set `required = true` to require one.

Arrays describe their elements with `items`. Use a type name as a shortcut, or a full field description:

```lua
labels = { type = "array", items = "string" },
scores = { type = "array", items = { type = "number" }, max_items = 5 },
```

Objects nest with `props`, which follows the same rules at every level:

```lua
input = {
  author = {
    type = "object",
    required = true,
    props = {
      name  = { type = "string", required = true },
      email = { type = "string" },
    },
  },
}
```

By default, an object rejects any field you did not describe. If a model invents an extra argument, the call fails with an error. To allow extra fields, set `strict = false` on the action:

```lua
agentd.action({
  name   = "notes.save",
  strict = false,   -- accept arguments beyond those described in `input`
  input  = { text = { type = "string", required = true } },
  handler = function(args, ctx) ... end,
})
```

Schemas work only in the table form of `agentd.action`. The two-argument short form (`agentd.action(name, handler)`) takes no schema.

### Current limitations

Schemas cover the common cases, and some JSON Schema features are not available yet. If you need one of these, check it inside the handler for now:

- **Types** are limited to `string`, `integer`, `number`, `boolean`, `array`, and `object`. There is no enum, no nullable type, and no "one of several types".
- **Strings** support `min_len` only. There is no maximum length, regular-expression pattern, or format such as email or date-time.
- **Numbers** have no minimum, maximum, or step constraint.
- **Arrays** support `items` and `max_items` only. There is no minimum count and no per-position element type.
- **Objects** either reject unknown fields (`strict = true`, the default) or allow them (`strict = false`). There is no per-field control.
- **Defaults and shared schemas** are not supported. Each schema is written inline on its action.
- **The `output` schema is hidden from models.** It is used only to check the handler's return value.
- **Validation errors are plain messages.** They name the failing path and are meant for people and logs.

You write schemas as the Lua tables shown above. Raw JSON Schema is not accepted.

## Permission slugs in `requires`

List every permission slug the handler uses at runtime. agent.d compares this list with the grants in `grants.toml`. If a listed permission is not granted, agent.d asks about it or refuses the call before the handler runs.

Common slugs:

| Slug | What it allows |
|---|---|
| `shell.exec:git` | Running the `git` binary via `ctx.shell` |
| `net:api.example.com` | HTTP/WebSocket to that host via `ctx.http` / `ctx.ws` |
| `fs.read:/tmp/**` | Reading paths under `/tmp` via `ctx.fs` |
| `fs.write:/tmp/**` | Writing paths under `/tmp` via `ctx.fs` |
| `secret:my_key` | Keyring access via `ctx.secret` |
| `memory.read:ns/**` | Reading a durable memory namespace via `ctx.memory` |
| `memory.write:ns/**` | Writing a durable memory namespace via `ctx.memory` |
| `ai:anthropic` | Model calls via `ctx.ai` |

See [Permission slugs](/v0/security/permission-slugs) for the full reference.

## `confirm = true`: interactive approval

Some actions are risky enough that a person should approve each call, for example pushing code or sending money. Mark them with `confirm = true`:

```lua
agentd.action({
  name    = "git.push",
  requires = { "shell.exec:git" },
  confirm = true,    -- every call is held for operator approval
  handler = function(args, ctx)
    return { exit_code = ctx.shell("git", { "push" }).exit_code }
  end,
})
```

Each call then waits for an approval (`allow_once`, `allow_forever`, or `deny`). If nobody answers, the call is refused. See [Interactive approvals](/v0/security/approvals) for who answers and when.

`ctx.call()` cannot call a `confirm` action. Only direct callers (runners, interfaces, services) can.

To let a confirm action run without a question, list it in [`[approval].skip`](/v0/security/grants#approval).

## Worked example: the git tool

The full `examples/tools/git.lua` shows the pattern end to end:

```lua
agentd.tool({
  name     = "git",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
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
  name     = "git.diff",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
  handler  = function(args, ctx)
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
  name     = "git.status",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
  handler  = function(args, ctx)
    local r = git(ctx, args, { "status", "--porcelain=v1" })
    return { status = r.output, exit_code = r.exit_code }
  end,
})
```

The matching `grants.toml` entry grants the permission:

```toml
[tool.git]
granted = ["shell.exec:git"]
```

## See also

- [Tools and actions concept](/v0/concepts/tools-and-actions)
- [ctx overview](/v0/writing/context)
- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)

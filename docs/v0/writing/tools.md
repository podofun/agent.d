# Writing Tools and Actions

A **tool** is a named namespace; **actions** are the callable operations inside
it. This page covers `agentd.tool` and `agentd.action`, the handler signature,
permission declarations, and the `confirm` flag — using the `git` tool as the
worked example.

## Registering a tool

```lua
agentd.tool({
  name    = "git",
  requires = { "shell.exec:git" },  -- permission: shell.exec:git
})
```

`requires` on a tool **declares** the permissions the tool's actions need as a
group. Declaring does not grant — grants live in
[grants.toml](/v0/security/grants). You can also declare `requires` on each action
individually (or on both; the union is checked).

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

When an action needs no metadata beyond a name and handler, use the two-argument
shorthand:

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
| `args` | `table \| nil` | Caller-supplied arguments. Always check for `nil` before indexing. |
| `ctx` | `table` | Per-invocation capability handle. See [ctx overview](/v0/writing/context). |

Return value must be a JSON-serializable Lua table (or `nil`). Returning a
non-serializable value (e.g. a function or userdata) is a runtime error.

## Describing arguments and results

By default a handler receives whatever arguments the caller sends, and it is up
to the handler to check them. You can instead describe the shape of an action's
arguments with an `input` table, and the shape of its result with an `output`
table. Both are optional.

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

Describing the arguments has two benefits:

- **Models call the action correctly.** When an AI runner is given the action
  as a tool, it sees the argument shape and fills in the right field names and
  types. Without an `input` table it only knows that the action takes "some
  object", and has to guess.
- **Bad calls are rejected early.** Arguments are checked against `input`
  before the handler runs, and the return value is checked against `output`
  after. If a check fails, the call returns a validation error that points at
  the offending field (for example, `/assignees: array exceeds maxItems 10`)
  and the handler never runs on bad input.

### Writing a schema

Each entry in an `input` (or `output`) table is one field. The key is the field
name; the value describes it:

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
| `"integer"` | — | Whole numbers only. |
| `"number"` | — | Any number. |
| `"boolean"` | — | |
| `"array"` | `items`, `max_items` | `items` is the element type. |
| `"object"` | `props` | `props` is a nested table of fields. |

Every field also accepts:

- `desc` — a human-readable description (shown to the model).
- `required` — whether the field must be present. **Fields are optional by
  default**; set `required = true` to require one.

Arrays describe their elements with `items`. Use a type name as shorthand, or a
full field spec:

```lua
labels = { type = "array", items = "string" },
scores = { type = "array", items = { type = "number" }, max_items = 5 },
```

Objects nest with `props`, which follows the same rules all the way down:

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

By default, an object rejects any field you did not describe — so if a model
invents an extra argument, the call fails instead of silently passing it
through. To allow extra fields, set `strict = false` on the action:

```lua
agentd.action({
  name   = "notes.save",
  strict = false,   -- accept arguments beyond those described in `input`
  input  = { text = { type = "string", required = true } },
  handler = function(args, ctx) ... end,
})
```

Schemas can only be declared in the table form of `agentd.action`. The
two-argument short form (`agentd.action(name, handler)`) takes no schema.

### Current limitations

Schema support covers the common cases; some JSON Schema features are not
available yet. If you need one of these, validate it inside the handler for now:

- **Types** are limited to `string`, `integer`, `number`, `boolean`, `array`,
  and `object`. There is no enum, no nullable type, and no "one of several
  types".
- **Strings** support `min_len` only — no maximum length, regular-expression
  pattern, or format such as email or date-time.
- **Numbers** have no minimum, maximum, or step constraint.
- **Arrays** support `items` and `max_items` only — no minimum count and no
  per-position element types.
- **Objects** can either reject unknown fields (`strict = true`, the default)
  or allow them (`strict = false`); there is no per-field control.
- **Defaults and shared/reusable schemas** are not supported — each schema is
  written inline on its action.
- **The `output` schema is not shown to models.** It is only used to validate
  the handler's return value.
- **Validation errors are plain messages**, not a structured per-field result.
  They name the offending path but are intended for humans and logs.

You always write the Lua table shown above; raw JSON Schema cannot be supplied
directly.

## Permission slugs in `requires`

List every permission slug the handler will use at runtime. The runtime
intersects these declared needs with the grants from `grants.toml` — if a
required permission is not granted, the action is denied before the handler
runs.

Common slugs:

| Slug | What it gates |
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

## `confirm = true` — interactive approval

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

When `confirm = true`, every invocation is sent to the approval plane and held
until an operator resolves it (`allow_once`, `allow_forever`, or `deny`). If no
operator is connected, the request times out and fails closed.

`ctx.call()` cannot invoke `confirm`-gated actions — only direct callers (runners,
interfaces, services) can trigger them.

[`[policy].auto_confirm`](/v0/security/grants) pre-approves specific action names so
they pass the `confirm` gate automatically.

## Worked example — the git tool

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

The corresponding `grants.toml` entry that actually enables the permission:

```toml
[tool.git]
granted = ["shell.exec:git"]
```

## See also

- [Tools and actions concept](/v0/concepts/tools-and-actions)
- [ctx overview](/v0/writing/context)
- [Permission slugs](/v0/security/permission-slugs)
- [grants.toml reference](/v0/security/grants)

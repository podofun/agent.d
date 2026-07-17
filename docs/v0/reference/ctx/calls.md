# ctx.call / ctx.run / ctx.structured — Cross-Component Calls

These three functions let one component invoke another: `ctx.call` dispatches an action, `ctx.run` drives a runner through its full tool-use loop, and `ctx.structured` extracts a validated JSON object from a runner's reply.

## Signatures

```lua
-- List registered action names
ctx.tools() -> string[]

-- Invoke another action
ctx.call(name: string, args?: table) -> any

-- Run a runner (string prompt or structured options)
ctx.run(name: string, prompt: string) -> RunResult
ctx.run(name: string, opts: {
  prompt?:   string,
  system?:   string,
  model?:    string,
  messages?: { role: string, content: string }[],
  history?:  { role: string, content: string }[],
}) -> RunResult

-- Run a runner and decode/validate JSON from its reply
ctx.structured(name: string, {
  prompt:    string,
  system?:   string,
  model?:    string,
  retries?:  integer,
  validate?: fun(value: any): boolean, string? | "inherit",
}) -> (decoded: table, response: RunResult)

-- Check a value against the enclosing action's declared output schema
ctx.validate_output(value: any) -> (ok: boolean, reason: string?)
```

## Methods

### `ctx.tools()`

Returns the names of all currently registered actions. Useful for introspection or building dynamic dispatchers.

**Returns:** `string[]`

### `ctx.call(name, args?)`

Invokes the action named `name` with the given `args` table. The callee's `requires` permissions are checked against the current grant context. Actions with `confirm = true` **cannot** be called via `ctx.call` — they require interactive approval and will be rejected.

| Parameter | Type | Description |
|---|---|---|
| `name` | `string` | Fully-qualified action name, e.g. `"git.status"`. |
| `args` | `table` | Arguments passed to the action handler. Optional. |

**Returns:** whatever the action handler returns.

### `ctx.run(name, prompt_or_opts)`

Runs the named runner with a prompt and drives its full tool-use loop until `stop_reason` is reached or `runtime.max_turns` is hit (default 16). Returns a `RunResult`.

| Field | Type | Description |
|---|---|---|
| `name` | `string` | Runner name registered with `agentd.runner`. |
| `prompt` | `string` | The user prompt. |
| `system` | `string` | Override the runner's system prompt. |
| `model` | `string` | Override the runner's model for this call. |
| `messages` | `table[]` | Full message history for the request. |
| `history` | `table[]` | Prior conversation turns to prepend. |

### `ctx.structured(name, opts)`

Like `ctx.run`, but instructs the runner to return a JSON object and decodes it. You can supply a `validate` function to check the decoded value; if validation fails the call is retried up to `retries` times.

| Field | Type | Description |
|---|---|---|
| `prompt` | `string` | The user prompt. |
| `system` | `string` | System prompt override. |
| `model` | `string` | Model override. |
| `retries` | `integer` | Number of retry attempts on decode or validation failure. |
| `validate` | `fun(value): boolean, string?` or `"inherit"` | Called with the decoded table; return `false, reason` to retry. The string `"inherit"` uses the enclosing action's `output` schema as the contract instead. |

**Returns:** `(decoded: table, response: RunResult)` — two values.

#### `validate = "inherit"`

When the action you're writing already declares an `output` schema, you don't
need to restate the same checks in a `validate` function. Pass
`validate = "inherit"` and the model's reply is validated against that schema —
same retry-with-reason loop, one single source of truth for the shape:

```lua
agentd.action({
  name = "geo.capital",
  input  = { country = { type = "string", required = true } },
  output = {
    capital = { type = "string", required = true },
    population_millions = { type = "number", required = true },
  },
  handler = function(args, ctx)
    local v = ctx.structured("geo_agent", {
      prompt = "Country: " .. args.country ..
        '. Reply as JSON: {"capital": ..., "population_millions": ...}',
      validate = "inherit",
    })
    return v  -- already schema-shaped; passes the action's output gate
  end,
})
```

If the reply doesn't match, the model is reprompted with the exact rejection
reason (e.g. `` /population_millions: expected number ``) until it conforms or
`retries` is exhausted. Using `"inherit"` in an action that declares no
`output` schema is an error — declare the schema first.

### `ctx.validate_output(value)`

Checks any value against the enclosing action's declared `output` schema.
Returns `true`, or `false, reason` on mismatch. This is what
`validate = "inherit"` uses under the hood; call it directly when you want to
pre-check a value you assembled yourself before returning it.

`ctx.structured` validates a runner's reply at the point you call it. To
validate an **action's own** arguments and return value — declaratively, and so
AI runners see the expected shape — describe them with `input`/`output`
schemas; see
[Describing arguments and results](/v0/writing/tools#describing-arguments-and-results).

## RunResult

| Field | Type | Description |
|---|---|---|
| `text` | `string` | The runner's final text response. |
| `provider` | `string` | Provider prefix used for the call. |
| `model` | `string` | Exact model string used. |
| `stop_reason` | `string \| nil` | Why the runner stopped (e.g. `"end_turn"`, `"max_turns"`). |

## Examples

```lua
-- List all actions and call one
agentd.action("meta.list_tools", function(args, ctx)
  return ctx.tools()
end)

agentd.action("git.summary", function(args, ctx)
  local diff = ctx.call("git.diff", { path = args.path })
  return diff
end)
```

```lua
-- Run a reviewer runner on the current diff
agentd.action("review.run", function(args, ctx)
  local result = ctx.run("backend_reviewer", args.prompt)
  return {
    review     = result.text,
    model      = result.model,
    stop_reason = result.stop_reason,
  }
end)
```

```lua
-- Extract structured data from a runner response
agentd.action("extract.tickets", function(args, ctx)
  local decoded, meta = ctx.structured("backend_reviewer", {
    prompt  = "Extract action items from this PR description:\n\n" .. args.description,
    retries = 2,
    validate = function(v)
      if type(v.items) ~= "table" then
        return false, "expected .items array"
      end
      return true
    end,
  })
  return { items = decoded.items, model = meta.model }
end)
```

::: warning confirm actions
`ctx.call` cannot invoke actions that have `confirm = true`. Calling one raises an error. Confirmed actions must be invoked directly by a client that can present the approval prompt.
:::

## See also

- [Concepts: tools and actions](/v0/concepts/tools-and-actions)
- [Concepts: runners](/v0/concepts/runners)
- [Writing tools: describing arguments and results](/v0/writing/tools#describing-arguments-and-results)
- [ctx.ai — inline model calls](/v0/reference/ctx/ai)
- [Security: approvals](/v0/security/approvals)

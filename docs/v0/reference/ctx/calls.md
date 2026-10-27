# ctx.call / ctx.run / ctx.structured: Cross-Component Calls

Use these functions when one action, runner or service needs another one. `ctx.call` calls an action. `ctx.run` runs a runner through its full tool-use loop. `ctx.structured` runs a runner and returns its reply as a validated JSON object.

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
  max_tokens?: integer,
  timeout_ms?: integer,
  session_id?: string,
  approval?: "human" | "classifier" | "nobody",
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

Returns the names of every registered action. Use it to inspect what is loaded or to build a dispatcher that picks an action at runtime.

**Returns:** `string[]`

### `ctx.call(name, args?)`

Calls the action named `name` with the `args` table. The called action's `requires` permissions are checked against the current grants. An action with `confirm = true` needs interactive approval, so `ctx.call` refuses it with an error.

| Parameter | Type | Description |
|---|---|---|
| `name` | `string` | Fully-qualified action name, e.g. `"git.status"`. |
| `args` | `table` | Arguments passed to the action handler. Optional. |

**Returns:** whatever the action handler returns.

### `ctx.run(name, prompt_or_opts)`

Runs the named runner with a prompt and keeps its tool-use loop going until the model gives a final answer. Returns a `RunResult`. The runner needs an `ai:<provider>` grant.

Outside a session, a run stops after `runtime.max_turns` turns (default 16) and the call fails with a `runner_turn_limit` error. A run with `session_id` has no turn limit and no default deadline, and keeps going until the model is done. The same [history validation, deadlines, concurrency limits, and optional usage result](/v0/reference/protocol#runners-run) apply to Lua and WebSocket calls.

| Field | Type | Description |
|---|---|---|
| `name` | `string` | Runner name registered with `agentd.runner`. |
| `prompt` | `string` | The user prompt. |
| `system` | `string` | Append additional system instructions. |
| `model` | `string` | Override the runner's model for this call. |
| `messages` | `table[]` | Full message history for the request. |
| `history` | `table[]` | Prior conversation turns to prepend. |
| `approval` | `string` | Who answers this run's questions: `"human"`, `"classifier"` or `"nobody"`. From Lua, this can make a run stricter and no looser. |

A run that `ctx.run` starts is at least as strict as the run that started it. If the parent run is `nobody`, the new run is `nobody` too. A value looser than the runner's setting fails with `approval_forbidden`. See [Where the setting comes from](/v0/security/approvals#where-the-setting-comes-from).

### `ctx.structured(name, opts)`

Works like `ctx.run`, and also tells the runner to reply with a JSON object, which it decodes for you. Pass a `validate` function to check the decoded value. When the check fails, the call asks the model again, up to `retries` times.

| Field | Type | Description |
|---|---|---|
| `prompt` | `string` | The user prompt. |
| `system` | `string` | System prompt override. |
| `model` | `string` | Model override. |
| `retries` | `integer` | Number of retry attempts on decode or validation failure. |
| `validate` | `fun(value): boolean, string?` or `"inherit"` | Called with the decoded table. Return `false, reason` to retry. The string `"inherit"` checks the value against the enclosing action's `output` schema. |

**Returns:** two values, `(decoded: table, response: RunResult)`.

#### `validate = "inherit"`

When your action already declares an `output` schema, pass `validate = "inherit"` to check the model's reply against that schema. The retry loop works the same way, and the shape is defined in one place:

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

If the reply does not match, the model is asked again with the reason it was rejected, for example `` /population_millions: expected number ``, until it matches or `retries` runs out. Using `"inherit"` in an action without an `output` schema raises an error, so declare the schema first.

### `ctx.validate_output(value)`

Checks any value against the enclosing action's declared `output` schema. Returns `true`, or `false, reason` when the value does not match. `validate = "inherit"` uses this check. Call it directly to check a value you built yourself before you return it.

`ctx.structured` checks a runner's reply where you call it. To check an action's own arguments and return value, describe them with `input` and `output` schemas. Runners then also see the expected shape. See [Describing arguments and results](/v0/writing/tools#describing-arguments-and-results).

## RunResult

| Field | Type | Description |
|---|---|---|
| `text` | `string` | The runner's final text response. |
| `provider` | `string` | Provider prefix used for the call. |
| `usage` | `table \| nil` | Cumulative provider-reported tokens when every turn reports usage; see [runner results](/v0/reference/protocol#runners-run). |
| `model` | `string` | Exact model string used. |
| `stop_reason` | `string \| nil` | Why the runner stopped, for example `"end_turn"` or `"max_tokens"`. |

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
`ctx.call` raises an error for actions that have `confirm = true`. A client that can show the approval prompt has to call those actions directly.
:::

## See also

- [Concepts: tools and actions](/v0/concepts/tools-and-actions)
- [Concepts: runners](/v0/concepts/runners)
- [Writing tools: describing arguments and results](/v0/writing/tools#describing-arguments-and-results)
- [ctx.ai: inline model calls](/v0/reference/ctx/ai)
- [Security: approvals](/v0/security/approvals)

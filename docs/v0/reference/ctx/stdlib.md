# Standard Library Extensions

agent.d augments Lua's standard library with a `json` global and additional methods on the built-in `string` library. These are **globals** — not under `ctx.*` — and are available in every file, including `init.lua`, action handlers, and service bodies.

**Required permission:** none.

## json

```lua
json.encode(value: any) -> string
json.decode(text: string, opts?: { nulls?: "sentinel" | "nil" }) -> any
json.null    -- userdata sentinel representing JSON null
json.is_null(value: any) -> boolean
```

### `json.encode(value)`

Serialize a Lua value to a JSON string. Tables with integer keys become JSON arrays; tables with string keys become JSON objects. Use `json.null` to represent an explicit JSON `null`.

**Returns:** `string`

### `json.decode(text, opts?)`

Parse a JSON string and return the corresponding Lua value.

| Option | Values | Description |
|---|---|---|
| `nulls` | `"sentinel"` | JSON `null` values are returned as `json.null` (userdata). Check with `json.is_null(v)`. This is the default when `nulls` is omitted. |
| `nulls` | `"nil"` | JSON `null` values are returned as Lua `nil`. Use with care in arrays or tables where `nil` collapses the structure. |

**Returns:** `any` — `string`, `number`, `boolean`, `table`, `json.null`, or `nil`.

### `json.null`

A userdata sentinel that round-trips through `json.encode`/`json.decode` as JSON `null`. Do not compare with `== nil`; use `json.is_null(v)` instead.

### `json.is_null(value)`

Returns `true` if `value` is `json.null`.

### Examples

```lua
-- Round-trip a table
local encoded = json.encode({ name = "alice", score = 42, tag = json.null })
-- '{"name":"alice","score":42,"tag":null}'

local decoded = json.decode(encoded)
-- decoded.name == "alice"
-- json.is_null(decoded.tag) == true
```

```lua
-- Preserve null in decoded output
local data = json.decode('{"value":null}', { nulls = "sentinel" })
if json.is_null(data.value) then
  print("value is explicitly null")
end
```

---

## String helpers

These methods are added to Lua's built-in `string` library. You can call them as `string.trim(s)` or using the method syntax `s:trim()`.

```lua
string.trim(s: string) -> string
string.ltrim(s: string) -> string
string.rtrim(s: string) -> string
string.startswith(s: string, prefix: string) -> boolean
string.endswith(s: string, suffix: string) -> boolean
string.contains(s: string, needle: string) -> boolean
string.split(s: string, sep?: string) -> string[]
```

### Methods

| Method | Description |
|---|---|
| `string.trim(s)` | Remove leading and trailing whitespace. |
| `string.ltrim(s)` | Remove leading whitespace only. |
| `string.rtrim(s)` | Remove trailing whitespace only. |
| `string.startswith(s, prefix)` | Return `true` if `s` begins with `prefix`. |
| `string.endswith(s, suffix)` | Return `true` if `s` ends with `suffix`. |
| `string.contains(s, needle)` | Return `true` if `needle` appears anywhere in `s`. |
| `string.split(s, sep?)` | Split `s` on the plain separator `sep`. When `sep` is omitted, split on any whitespace. Returns `string[]`. |

### Examples

```lua
-- Trim and check a user-supplied value
local input = "  hello world  "
local clean = input:trim()           -- "hello world"
local words = string.split(clean)    -- { "hello", "world" }
```

```lua
-- Route based on prefix
local cmd = "git status --short"
if string.startswith(cmd, "git") then
  ctx.log.info("git command detected")
end
```

```lua
-- Split a CSV line
local line = "alice,42,engineer"
local parts = string.split(line, ",")
-- parts[1] == "alice", parts[2] == "42", parts[3] == "engineer"
```

## See also

- [ctx — overview](/v0/reference/ctx/)
- [Concurrency](/v0/reference/ctx/concurrency)
- [Writing tools](/v0/writing/tools)
- [ctx.http](/v0/reference/ctx/http)

# ctx.fs — Filesystem

`ctx.fs` provides read and write access to the local filesystem. All paths are resolved (symlinks expanded, `..` collapsed) before the permission check, so grants cannot be bypassed through aliases or traversal tricks.

**Required permissions:** `fs.read:<glob>` for reads; `fs.write:<glob>` for writes, appends, and removes.

## Signatures

```lua
ctx.fs.read(path: string) -> string
ctx.fs.write(path: string, content: string)
ctx.fs.append(path: string, content: string)
ctx.fs.exists(path: string) -> boolean
ctx.fs.stat(path: string) -> table
ctx.fs.list_dir(path: string) -> table[]
ctx.fs.remove(path: string)
```

## Methods

| Method | Permission | Description |
|---|---|---|
| `ctx.fs.read(path)` | `fs.read:<path>` | Read the entire file and return its contents as a string. |
| `ctx.fs.write(path, content)` | `fs.write:<path>` | Write `content` to `path`, replacing any existing content. |
| `ctx.fs.append(path, content)` | `fs.write:<path>` | Append `content` to `path`. |
| `ctx.fs.exists(path)` | `fs.read:<path>` | Return `true` if the path exists. |
| `ctx.fs.stat(path)` | `fs.read:<path>` | Return a table of file metadata. |
| `ctx.fs.list_dir(path)` | `fs.read:<path>` | Return an array of entry tables for the directory. |
| `ctx.fs.remove(path)` | `fs.write:<path>` | Delete the file or empty directory at `path`. |

## Parameters

Each method takes a `path` string. Paths may be absolute or relative; relative paths are resolved from the daemon's working directory. Wildcard grants (`fs.read:/tmp/**`) allow any sub-path below the prefix.

Grant example in `grants.toml`:

```toml
[tool.notes]
granted = ["fs.read:/home/user/notes/**", "fs.write:/home/user/notes/**"]
```

## Examples

```lua
-- Read a config file and parse it as JSON
agentd.action("config.load", function(args, ctx)
  local content = ctx.fs.read(args.path)
  return json.decode(content)
end)
```

```lua
-- Append a timestamped log entry
agentd.action("log.append", function(args, ctx)
  ctx.fs.append("/var/log/agentd-audit.log", args.entry .. "\n")
end)
```

```lua
-- List files in a directory, only if they exist
agentd.action("dir.list", function(args, ctx)
  if not ctx.fs.exists(args.path) then
    return { entries = {} }
  end
  local entries = ctx.fs.list_dir(args.path)
  return { entries = entries }
end)
```

::: info Path resolution
Symlinks and `..` segments in the path are resolved to their real absolute path before the permission check. A grant of `fs.read:/data/**` cannot be bypassed by passing `/data/../etc/passwd`.
:::

## See also

- [Security: permission slugs](/v0/security/permission-slugs)
- [Security: grants](/v0/security/grants)
- [ctx.shell](/v0/reference/ctx/shell)
- [ctx — overview](/v0/reference/ctx/)

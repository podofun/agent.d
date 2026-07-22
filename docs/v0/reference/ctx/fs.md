# ctx.fs — Filesystem

Read and write files from inside an action or runner.

```lua
agentd.action{
  name = "notes.save",
  requires = { "fs.write:notes/**" },
  handler = function(args, ctx)
    ctx.fs.write("notes/" .. args.title .. ".md", args.body)
  end,
}
```

Every call needs a matching grant in `grants.toml`. Reads need `fs.read:<path>`; writes, appends, and deletes need `fs.write:<path>`. Without the grant, the call will throw an error before touching disk.

## Methods

| Method                         | Grant needed      | Returns   | What it does                                                  |
| ------------------------------ | ----------------- | --------- | ------------------------------------------------------------- |
| `ctx.fs.read(path)`            | `fs.read:<path>`  | `string`  | Read the whole file as a string.                              |
| `ctx.fs.write(path, content)`  | `fs.write:<path>` | —         | Write `content`, replacing the file. Creates parent folders.  |
| `ctx.fs.append(path, content)` | `fs.write:<path>` | —         | Add `content` to the end of the file (creates it if missing). |
| `ctx.fs.exists(path)`          | `fs.read:<path>`  | `boolean` | `true` if the path exists.                                    |
| `ctx.fs.stat(path)`            | `fs.read:<path>`  | `table`   | File metadata — see [stat](#stat).                            |
| `ctx.fs.list_dir(path)`        | `fs.read:<path>`  | `table[]` | List a directory — see [list_dir](#list-dir).                 |
| `ctx.fs.remove(path)`          | `fs.write:<path>` | —         | Delete a file.                                                |

A missing file makes `read`, `stat`, and `list_dir` throw and error. It's advised that you guard with `exists` first if the file might not be there.

### stat

```lua
local s = ctx.fs.stat("notes/todo.md")
-- {
--   path          = "/home/me/agent/notes/todo.md",  -- absolute
--   kind          = "file",   -- "file" | "dir" | "symlink" | "other"
--   size          = 1240,     -- bytes
--   readonly      = false,
--   modified_unix = 1720000000,  -- seconds since epoch (absent if unknown)
-- }
```

### list_dir

Returns one row per entry, sorted by name:

```lua
for _, e in ipairs(ctx.fs.list_dir("notes")) do
  print(e.name, e.kind)   -- e = { name, path (absolute), kind }
end
```

## Paths and the working directory

A `path` can be absolute or relative.

- **Absolute** (`/home/me/data.txt`, `C:\data\x.txt`) is used as-is.
- **Relative** (`notes/todo.md`) is joined onto the current **working directory**.

By default, the working directory is the folder that contains `init.lua`. Thus, `ctx.fs.read("notes/todo.md")` reads `<configuration-directory>/notes/todo.md`. You can use an absolute path to read a file outside the configuration directory, such as a system log.

### Setting the working directory

The working directory is decided per call, using the first of these that applies:

1. `cwd` on the action itself — `agentd.action{ cwd = "..." }`
2. `cwd` on the runner that called the action
3. the working directory it inherited from the action that called it
4. the workspace root (the default)

A relative `cwd` is always joined onto the workspace root, so even when the action inherited a working directory, it does not stack on top of it. An action that inherits `<workspace>/a` and declares `cwd = "b"` runs in `<workspace>/b`, not `<workspace>/a/b`. An absolute `cwd` is used as-is.

```lua
-- Every relative path in this action is rooted at <workspace>/reports.
agentd.action{
  name = "report.write",
  cwd = "reports",
  requires = { "fs.write:reports/**" },
  handler = function(args, ctx)
    ctx.fs.write("today.md", args.body)   -- writes <workspace>/reports/today.md
  end,
}
```

Nested calls inherit the caller's working directory unless they set their own. A runner with `cwd = "workspace/acme"` runs all its tools rooted there, and a single tool can override just itself with its own `cwd`.

### Changing it at runtime

```lua
ctx.fs.getcwd()             -- current working directory (absolute string)
ctx.fs.chdir(dir)           -- switch to `dir`, returns the new absolute path
ctx.fs.with_cwd(dir, fn)    -- run fn() with the working directory set to `dir`
```

`chdir` is like `cd` in a shell: absolute `dir` switches there, relative `dir` moves relative to where you already are. It stays in effect for the rest of the call.

`with_cwd` runs `fn` in `dir` and puts the working directory back afterwards — even if `fn` errors or waits on I/O. Reach for it when you want a temporary switch:

```lua
local readme = ctx.fs.with_cwd("vendor/lib", function()
  return ctx.fs.read("README.md")   -- vendor/lib/README.md
end)
-- working directory is back to what it was here
```

## Grants follow the same paths

Because grants are checked against the final absolute path, a **relative grant resolves against the same working directory as the call**. That keeps `grants.toml` portable:

```toml
[tool.notes]
# Rooted at the workspace — works on any machine.
granted = ["fs.read:notes/**", "fs.write:notes/**"]
```

An absolute grant still works and is left exactly as written:

```toml
[tool.backups]
granted = ["fs.write:/var/backups/agentd/**"]
```

Use `**` to cover a whole subtree and `*` for a single level: `fs.read:notes/**` covers `notes/a/b.md`, while `fs.read:notes/*` covers `notes/a.md` only.

## Safety

Whatever path a tool passes, agentd first works out the real file it points to — it follows any symlinks and resolves `..` segments down to the actual location on disk. It then checks that real location against the tool's grants and, if allowed, reads or writes that same real location.

This is what confines the tool. A path that a tool builds from model output or untrusted input can't reach past its grants: a symlink pointing somewhere ungranted, or a `../` that climbs out of a granted folder, both resolve to their real target — and the real target is what gets checked. So a `fs.read:/data/**` grant stays scoped to `/data`, no matter what the tool hands in:

```lua
-- Tool granted only fs.read:/data/**
ctx.fs.read("/data/../etc/passwd")   -- resolves to /etc/passwd → denied
```

`chdir` plays by the same rule. It only moves where a tool's relative paths start from — it hands out no new access. After a `chdir`, each path is still resolved to its real location and checked against the tool's grants, so a tool can't `chdir` into a granted folder and then `../` its way out.

## More examples

```lua
-- Read JSON config relative to the workspace
agentd.action{
  name = "config.load",
  requires = { "fs.read:config/**" },
  handler = function(_, ctx)
    return json.decode(ctx.fs.read("config/app.json"))
  end,
}
```

```lua
-- Append an audit line, creating the log on first write
agentd.action{
  name = "audit.log",
  requires = { "fs.write:logs/**" },
  handler = function(args, ctx)
    ctx.fs.append("logs/audit.log", args.entry .. "\n")
  end,
}
```

```lua
-- Only list a directory if it's there
agentd.action{
  name = "dir.list",
  requires = { "fs.read:data/**" },
  handler = function(args, ctx)
    if not ctx.fs.exists(args.dir) then return { entries = {} } end
    return { entries = ctx.fs.list_dir(args.dir) }
  end,
}
```

## See also

- [Security: permission slugs](/v0/security/permission-slugs)
- [Security: grants](/v0/security/grants)
- [ctx.shell](/v0/reference/ctx/shell)
- [ctx — overview](/v0/reference/ctx/)

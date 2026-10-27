# ctx.fs: Filesystem

Read and write files from inside an action or runner. Lua strings carry arbitrary bytes, including NUL and non-UTF-8 data; the same methods handle text and binary files.

```lua
agentd.action{
  name = "notes.save",
  requires = { "fs.write:notes/**" },
  handler = function(args, ctx)
    ctx.fs.write("notes/" .. args.title .. ".md", args.body)
  end,
}
```

Every call needs a matching grant in `grants.toml`. Reads need `fs.read:<path>`; writes, appends, and deletes need `fs.write:<path>`. Without the grant, the call raises an error before it touches the disk.

## Methods

| Method                         | Grant needed      | Returns   | What it does                                                  |
| ------------------------------ | ----------------- | --------- | ------------------------------------------------------------- |
| `ctx.fs.read(path)`            | `fs.read:<path>`  | `string`  | Read the whole file as a binary-safe Lua string.                              |
| `ctx.fs.write(path, content)`  | `fs.write:<path>` | `integer` | Write `content`, replacing the file. Creates parent folders.  |
| `ctx.fs.append(path, content)` | `fs.write:<path>` | `integer` | Append bytes, creating the file and parent folders if missing. |
| `ctx.fs.exists(path)`          | `fs.read:<path>`  | `boolean` | `true` if the path exists.                                    |
| `ctx.fs.stat(path)`            | `fs.read:<path>`  | `table`   | File metadata. See [stat](#stat).                             |
| `ctx.fs.list_dir(path)`        | `fs.read:<path>`  | `table[]` | List a directory. See [list_dir](#list-dir).                  |
| `ctx.fs.remove(path)`          | `fs.write:<path>` | `integer` | Delete a file.                                                |
| `ctx.fs.history(path)` | `fs.read:<path>` | `table` | Current revision and all retained revision metadata. |
| `ctx.fs.diff(path, revision)` | `fs.read:<path>` | `table` | Binary splice for one revision. |
| `ctx.fs.undo(path)` | `fs.write:<path>` | `integer` | Restore the current revision's parent. |
| `ctx.fs.redo(path)` | `fs.write:<path>` | `integer` | Reapply the most recently undone revision. |
| `ctx.fs.restore(path, revision)` | `fs.write:<path>` | `integer` | Restore any retained revision, including zero. |
| `ctx.fs.edit(path, old, new, opts?)` | `fs.write:<path>` | `integer` | Replace exact text in a file you have read. See [Changing part of a file](#changing-part-of-a-file). |
| `ctx.fs.patch(patch, opts?)` | `fs.read` and `fs.write` for each file | `table[]` | Apply a unified diff to the files it names, all or nothing. |

A missing file makes `read`, `stat` and `list_dir` raise an error. If the file might not be there, check with `exists` first.

## Changing part of a file

Use `ctx.fs.edit` and `ctx.fs.patch` when you want to change a few lines and keep the rest of the file as it is. `edit` replaces one piece of text. `patch` applies a unified diff, the format `git diff` prints and models write.

```lua
-- Requires fs.read:src/** and fs.write:src/**.
local source = ctx.fs.read("src/main.rs")
ctx.fs.edit("src/main.rs", "println!(\"hi\")", "println!(\"hello\")")
```

Before you change part of a file, read it in the same chat session, or in the same run outside a session. If the file changed after you read it, for example because your editor or a shell command saved it, the call stops and asks you to read it again. This keeps edits tied to the file as it is on disk now.

Writing a file with `ctx.fs.write` counts as reading it, so you can write a file and then edit it. A plain `ctx.fs.write` works without a read, because it replaces the whole file.

`edit` replaces `old` with `new`. By default `old` must appear exactly once. If it appears more than once, the error tells you how many times, and you can add surrounding text to make it unique. Pass `{ replace_all = true }` to replace every occurrence. Files with Windows line endings keep them.

`patch` takes diff text and returns `{ { path = ..., revision = ... }, ... }` for the files it changed:

```lua
local changed = ctx.fs.patch([[
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 fn main() {
-    println!("hi");
+    println!("hello");
 }
]])
```

`patch` checks every file and works out every change before it writes anything. Either every file in the patch changes or none does. When one part does not apply, the error names the file, the hunk and the line that differs.

A patch can create a file (`--- /dev/null`) and remove one (`+++ /dev/null`). A patch that renames a file, changes its permissions or carries binary data fails as a whole. To apply a one-file diff to a different path, pass `{ path = "other.rs" }`.

Edits and patches go into the file's history like any other write, so `undo` and `restore` work on them. To build or inspect diffs in memory, use [`ctx.diff`](/v0/reference/ctx/diff).

## Binary diffs and undo/redo

Each successful `write`, `append` and `remove` records what changed and returns a revision ID for that file. You can use these IDs to look at a change, undo it, or go back to any earlier version. Reads and metadata queries leave the history alone.

Revision `0` is the file as it was before its first tracked change, including whether it existed at all. An empty file and a missing file are different states.

```lua
-- Requires both fs.read:images/** and fs.write:images/**.
local first = ctx.fs.write("images/sample.bin", string.char(0, 255, 128))
local second = ctx.fs.append("images/sample.bin", string.char(254))
ctx.fs.undo("images/sample.bin")          -- returns first
ctx.fs.redo("images/sample.bin")          -- returns second
ctx.fs.restore("images/sample.bin", 0)    -- restores the original file, or removes a new file
ctx.fs.restore("images/sample.bin", second)

local change = ctx.fs.diff("images/sample.bin", second)
-- change.revision.offset == 3 (zero-based byte offset)
-- change.removed == ""
-- change.added == string.char(254)
```

`history(path)` returns `{ current = id, revisions = { ... } }`. Each revision has `id`, `parent`, `operation` (`write`, `append` or `remove`), `timestamp_ms`, `offset`, `removed_bytes`, `added_bytes`, `before_size` and `after_size`. A missing size means the file did not exist on that side of the change.

`diff(path, id)` returns `{ revision = metadata, removed = bytes, added = bytes }`. To apply the change, replace the `removed` bytes at the zero-based `offset` with `added`. To undo it, do the reverse.

### How undo and redo move

`undo` and `redo` step along the current line of revisions. If you write after an undo, the redo stack is cleared, and every earlier revision stays in the history, so `restore` can still reach it. `restore` also clears the redo stack. An unknown revision ID, or an undo or redo with nothing left to step to, raises an error. Revision IDs belong to one file, identified by its full resolved path.

### What history covers

History is shared by actions, runners and services. It lasts as long as the daemon runs and survives hot reloads. Restarting the daemon clears it.

History also records what other programs change. Around every action call that may write files, agent.d looks at the call's folder before and after the call: the chat's folder, or the runner's `cwd`. A file that a shell command, a formatter or a script created, changed or removed during the call gets a revision of its own, which you can undo like any other. Each revision names the action and run that made it. A call with no chat folder and no runner `cwd` works in the configuration folder, and nothing is recorded there.

These folder checks skip what your `.gitignore` files ignore, as well as `.git`, `target` and `node_modules`. A file over 1 MiB, or one that is not text, is recorded with its size only and cannot be undone. A folder with more than 50,000 files is not checked, and symlinks are not followed. If another call is already working in the same folder for more than two seconds, the new call is not recorded, and its trace event says so.

When a file changes outside agent.d, for example in your editor, history keeps that edit. The next change from agent.d works once the file has been read again, and an undo stops before it would go past your edit, so your edit stays. History compares a file's contents only. Changing its permissions, for example with `chmod`, does not count as an edit.

History works on regular files. Directories, special files and files with more than one hard link on Unix are refused. A symlink resolves to its target before the grant check. A file keeps its permissions when it changes. History records file contents only: ownership, ACLs, extended attributes, timestamps and inode identity are not recorded. It is a content history for undo, and it does not replace backups or a filesystem snapshot.

Undoing the creation of a file also removes any parent folders that write created, as long as they are empty.

### Cost

Computing a change takes time in proportion to the file size. History stores only the span between the first and last changed byte, so two edits far apart in one write keep a large span. Every write and replay builds the complete new file before swapping it in, so an append also costs time in proportion to the file size. History stays in memory and is not trimmed automatically, and replaying a revision takes longer for bigger files and for revisions further back.

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

By default, the working directory is the folder that contains `init.lua`. This is your configuration folder, the one agent.d reads its settings from. So `ctx.fs.read("notes/todo.md")` reads `notes/todo.md` in that folder. Use an absolute path to read a file somewhere else, such as a system log.

### Setting the working directory

The working directory is decided per call, using the first of these that applies:

1. `cwd` on the action itself: `agentd.action{ cwd = "..." }`
2. `cwd` on the runner that called the action
3. the folder of the chat session the run belongs to, if it has one
4. the working directory it inherited from the action that called it
5. the configuration folder (the default)

A relative `cwd` is joined onto the configuration folder, even when the action inherited a different working directory. An action that inherits `<config>/a` and declares `cwd = "b"` runs in `<config>/b`. An absolute `cwd` is used as written.

```lua
-- Every relative path in this action is rooted at <config>/reports.
agentd.action{
  name = "report.write",
  cwd = "reports",
  requires = { "fs.write:reports/**" },
  handler = function(args, ctx)
    ctx.fs.write("today.md", args.body)   -- writes <config>/reports/today.md
  end,
}
```

Nested calls inherit the caller's working directory unless they set their own. A runner with `cwd = "projects/acme"` runs all its tools rooted there, and a single tool can override just itself with its own `cwd`.

### Changing it at runtime

```lua
ctx.fs.getcwd()             -- current working directory (absolute string)
ctx.fs.chdir(dir)           -- switch to `dir`, returns the new absolute path
ctx.fs.with_cwd(dir, fn)    -- run fn() with the working directory set to `dir`
```

`chdir` is like `cd` in a shell: absolute `dir` switches there, relative `dir` moves relative to where you already are. It stays in effect for the rest of the call.

`with_cwd` runs `fn` in `dir` and puts the working directory back afterwards, also when `fn` raises an error or waits on I/O. Use it for a temporary switch:

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
# Rooted at the configuration folder, so it works on any machine.
granted = ["fs.read:notes/**", "fs.write:notes/**"]
```

An absolute grant still works and is left exactly as written:

```toml
[tool.backups]
granted = ["fs.write:/var/backups/agentd/**"]
```

Use `**` to cover a whole subtree and `*` for a single level: `fs.read:notes/**` covers `notes/a/b.md`, while `fs.read:notes/*` covers `notes/a.md` only.

## Safety

Whatever path a tool passes, agentd first works out the real file it points to. It follows symlinks and resolves `..` segments down to the actual location on disk. It then checks that location against the tool's grants and, if allowed, reads or writes that same location.

This keeps a tool inside its grants, even when it builds paths from model output or untrusted input. A symlink that points somewhere ungranted, or a `../` that climbs out of a granted folder, resolves to its real target, and agentd checks that target. A `fs.read:/data/**` grant stays scoped to `/data`:

```lua
-- Tool granted only fs.read:/data/**
ctx.fs.read("/data/../etc/passwd")   -- resolves to /etc/passwd → denied
```

`chdir` follows the same rule. It moves the point where a tool's relative paths start and grants no new access. After a `chdir`, each path is still resolved to its real location and checked against the tool's grants, so a tool that moves into a granted folder cannot `../` its way out.

## More examples

```lua
-- Read JSON settings from the configuration folder
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
- [ctx overview](/v0/reference/ctx/)

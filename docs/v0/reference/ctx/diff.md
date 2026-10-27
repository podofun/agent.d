# ctx.diff: Unified Diffs

`ctx.diff` creates, reads and applies unified diffs, the format `git diff` prints, `git apply` and `patch` accept, and language models write when they change code. It works on text only and never touches files, so it needs no grant. To apply a diff to files on disk, use [`ctx.fs.patch`](/v0/reference/ctx/fs#changing-part-of-a-file).

A common use is showing a change before making it, or adjusting a diff a model wrote before applying it.

## Methods

| Method | Returns | What it does |
| --- | --- | --- |
| `ctx.diff.create(old, new, opts?)` | `string` | A diff from `old` to `new`. `opts.path` names the file (default `"file"`), and `opts.context` sets how many unchanged lines surround each change (default `3`). Identical texts give an empty string. |
| `ctx.diff.parse(text)` | `table[]` | The diff as one table per file. |
| `ctx.diff.format(files)` | `string` | Turn parsed tables back into diff text. |
| `ctx.diff.apply(text, patch)` | `string` | Apply a one-file diff, as text or one parsed table, to `text`. An empty diff returns `text` unchanged. |

## Reading and changing a diff

`parse` returns a list with one table per file:

```lua
local files = ctx.diff.parse(diff_text)
-- files[1].old_path  "src/main.rs" (nil when the diff creates the file)
-- files[1].new_path  "src/main.rs" (nil when the diff removes the file)
-- files[1].hunks[1]  { old_start, old_lines, new_start, new_lines, lines }
```

Each entry in `lines` keeps its first character: a space for an unchanged line, `-` for a removed line, `+` for an added one. You can change the tables and turn them back into text:

```lua
local files = ctx.diff.parse(ctx.diff.create("a\nb\n", "a\nc\n", { path = "notes.txt" }))
files[1].hunks[1].lines[3] = "+z"           -- add "z" instead of "c"
local adjusted = ctx.diff.format(files)
local result = ctx.diff.apply("a\nb\n", adjusted)   -- "a\nz\n"
```

## When a diff does not apply

`apply` matches each change exactly, where the diff says or, if the text has shifted, at the one place its lines appear. When a change does not match, the error names the hunk and the line that differs, for example "hunk 1 of `notes.txt` does not apply, line 2 reads `q` but the patch expects `b`". Each line keeps its own ending, so files with Windows line endings stay that way.

## See also

- [ctx.fs](/v0/reference/ctx/fs): `ctx.fs.edit` and `ctx.fs.patch` change files and record history

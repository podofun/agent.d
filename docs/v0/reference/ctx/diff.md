# ctx.diff: Unified Diffs

Use `ctx.diff` when you want to show a change before you make it, or adjust a diff a model wrote before you apply it. It creates, reads and applies unified diffs, the format that `git diff` prints, that `git apply` and `patch` accept, and that language models write when they change code.

`ctx.diff` works on strings in memory and does not read or write files, so it needs no grant. To apply a diff to files on disk, use [`ctx.fs.patch`](/v0/reference/ctx/fs#changing-part-of-a-file).

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

Each entry in `lines` keeps its first character: a space for an unchanged line, `-` for a removed line and `+` for an added one. You can edit these tables and turn them back into text with `format`:

```lua
local files = ctx.diff.parse(ctx.diff.create("a\nb\n", "a\nc\n", { path = "notes.txt" }))
files[1].hunks[1].lines[3] = "+z"           -- add "z" instead of "c"
local adjusted = ctx.diff.format(files)
local result = ctx.diff.apply("a\nb\n", adjusted)   -- "a\nz\n"
```

## When a diff does not apply

`apply` looks for each change at the line number the diff gives. If the text has shifted, it uses the one place where the change's lines appear. When a change cannot be placed, `apply` raises an error that names the hunk and the line that differs, for example "hunk 1 of `notes.txt` does not apply, line 2 reads `q` but the patch expects `b`".

Each line keeps its own line ending, so a file with Windows line endings keeps them after the diff is applied.

## See also

- [ctx.fs](/v0/reference/ctx/fs): `ctx.fs.edit` and `ctx.fs.patch` change files and record history

# agentd-fs

Filesystem primitive.

`read_to_string`, `read_bytes`, `write`, `append`, `exists`, `stat`, `list_dir`,
`remove_file`, `remove_dir_all`.

No permission checks here — the caller (scripting `ctx.fs`) gates by
`fs.read:<abs-path>` / `fs.write:<abs-path>`.

# agentd-fs

`agentd-fs` provides asynchronous filesystem primitives for the runtime.

It can read text or bytes, write or append files, inspect paths, list directories, and remove files or directory trees. Directory entries and file metadata use typed return values.

This crate performs filesystem operations. Callers must complete permission checks and path confinement before they call it.

`History` adds a shared, in-memory journal for regular-file writes, appends, and removals. Each operation records a reversible binary splice; unchanged prefixes and suffixes are not retained. Cloned handles share per-path locks and revision trees. Undo, redo, and restore preserve abandoned branches, verify the current bytes and permissions before replay, and atomically replace file contents. Revision zero is the state before the first tracked mutation.

History does not enforce permissions: callers must authorize reads of history/diffs and writes for mutations/replay using the resolved absolute path. It rejects symlinks, special files, and, on Unix, multiply linked files. The scripting layer resolves symlink targets before authorization. File modes are preserved; inode identity, ownership, ACLs, extended attributes, and timestamps are not versioned. History is not a filesystem snapshot or a crash-recovery journal.

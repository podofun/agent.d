# agentd-fs

`agentd-fs` provides asynchronous filesystem primitives for the runtime.

It can read text or bytes, write or append files, inspect paths, list directories, and remove files or directory trees. Directory entries and file metadata use typed return values.

This crate performs filesystem operations. Callers must complete permission checks and path confinement before they call it.

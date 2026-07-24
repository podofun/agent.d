# agentd-packages

`agentd-packages` installs and loads agentd packages.

It provides:

- Typed parsing for `package.toml`.
- Git-based install, update, and update checks.
- A local package index.
- Expansion of package paths and declared grant scopes.

A package manifest declares requested permissions. The grant file remains authoritative. Grant expansion occurs only when the package has `trusted = true`, and explicit component grants take precedence.

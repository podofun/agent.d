# agentd-packages

Package — a git-installed bundle of tools/actions/runners/services with a single
declared permission set.

- `Manifest` (`package.toml`), `PackageIndex` (`index.toml` provenance + commit pin).
- `install` / `update` / `update_check` — shell out to `git`.
- `expand_grants(&[LoadedPackage], &mut GrantsFile)` — the desugaring pass that turns a **trusted** package into per-tool/runner/service grant rows.

Approach A: packages desugar into rows the 5-layer engine already enforces. The engine is **never modified**. Default-deny holds — no `trusted = true`, no grants.

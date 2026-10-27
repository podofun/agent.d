# Managing packages

Use `agentctl packages` to install, update, and remove packages. These commands work on local files and git, so the daemon does not need to be running.

## Listing installed packages

To see what is installed, run:

```bash
agentctl packages ls
```

Shows all packages recorded in `$XDG_DATA_HOME/agentd/packages/index.toml`.

## Installing a package

```bash
agentctl packages install <git-url>
agentctl packages install <git-url> --ref <ref>
```

`--ref` accepts any git ref (branch, tag, or commit SHA). Without it, agentctl uses the default branch.

```bash
# Install from the default branch
agentctl packages install https://github.com/example/acme-agentd

# Pin to a specific tag
agentctl packages install https://github.com/example/acme-agentd --ref v0.2.0
```

After cloning, `agentctl` reads `package.toml` and **prints the declared `permissions`** so you can review them before approving.

::: warning
A newly installed package has no permissions until you add `[package.<name>] trusted = true` to `grants.toml`. Review the printed permission list first.
:::

## Approving after install

Once you have reviewed the declared permissions, add an entry to `grants.toml`:

```toml
[package.acme]
trusted = true
```

Then restart the daemon (or trigger a hot reload with `--watch`) to load the package.

## Updating a package

```bash
agentctl packages update <name>
```

Pulls the latest commit on the ref the package tracks and updates the local copy. Review the package changelog and any permission changes before you restart the daemon.

## Removing a package

```bash
agentctl packages remove <name>
```

Deletes the package directory and removes its entry from `index.toml`. Remove the matching `[package.<name>]` entry from `grants.toml` as well.

## Storage layout

Packages are stored under:

```
$XDG_DATA_HOME/agentd/packages/
├── index.toml          # registry of installed packages
├── acme/               # cloned package directory
│   ├── package.toml
│   └── main.lua
└── …
```

`$XDG_DATA_HOME` defaults to `~/.local/share` on Linux if the variable is unset.

## See also

- [Packages overview](/v0/packages/)
- [Authoring packages](/v0/packages/authoring)
- [grants.toml reference](/v0/security/grants)
- [CLI reference](/v0/reference/cli)

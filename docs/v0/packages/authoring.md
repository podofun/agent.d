# Authoring packages

A package is a directory containing a `package.toml` manifest and one or more Lua files. This page walks through the layout and conventions using the `acme` example from the repository.

## Directory layout

```
acme/
├── package.toml   # manifest: name, version, entry, permissions
└── main.lua       # entry point (or whatever `entry` names)
```

You can have as many additional Lua files as you like; use `import(...)` inside `main.lua` to load them.

## `package.toml`

The manifest has two sections.

### `[package]`

```toml
[package]
name    = "acme"
version = "0.1.0"
entry   = "main.lua"
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | The package name. Used as the namespace prefix (`acme/...`). |
| `version` | yes | Semver string. |
| `entry` | yes | Path to the Lua entry point, relative to the package directory. |

### `permissions`

```toml
permissions = [
  "net:api.acme.com",
  "shell.exec:git",
]
```

This is a **flat list of permission slugs** that covers every component the package registers. It is a declaration, not a grant — the operator must approve it with `[package.acme] trusted = true` in `grants.toml` before any permission takes effect.

::: warning
Declare only what your package actually needs. Operators review this list before trusting a package; a bloated permission set reduces trust.
:::

## Entry point (`main.lua`)

The entry point registers components using the standard `agentd.*` API. Everything registered here is automatically prefixed with the package name at load time.

```lua
-- examples/packages/acme/main.lua

agentd.tool{ name = "git", requires = { "shell.exec:git" } }

agentd.action("git.diff", function(args, ctx)
  local out = ctx.shell("git", { "diff" }, { cwd = args.cwd })
  return { diff = out.stdout }
end)

agentd.runner{
  name    = "reviewer",
  model   = "anthropic/claude-opus-4-7",
  -- unqualified action names are rewritten to the package-qualified form
  actions = { "git.diff" },   -- resolved to "acme/git.diff" at registration
}
```

## Auto-prefixing rules

| What you write | What gets registered |
|---|---|
| `agentd.tool{ name = "git" }` | `acme/git` |
| `agentd.action("git.diff", …)` | `acme/git.diff` |
| `agentd.runner{ name = "reviewer" }` | `acme/reviewer` |
| `actions = { "git.diff" }` inside a runner | resolved to `acme/git.diff` |

Callers reference the fully-qualified name: `agentctl call acme/git.diff`.

## What the manifest does not do

The `permissions` list in `package.toml` **never self-grants**. It is metadata that the operator reads at install time and then approves (or not) in `grants.toml`. The package cannot bypass the permission engine.

## See also

- [Managing packages](/v0/packages/managing)
- [Packages overview](/v0/packages/)
- [grants.toml reference](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)

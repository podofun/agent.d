# Authoring packages

Write a package when you want to share a set of tools, runners or services with other agent.d users. A package is a directory with a `package.toml` manifest and one or more Lua files. This page walks through the layout using the `acme` example from the repository.

## Directory layout

```
acme/
├── package.toml   # manifest: name, version, entry, permissions
└── main.lua       # entry point (or whatever `entry` names)
```

You can add as many Lua files as you like. Load them with `import(...)` inside `main.lua`.

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

This is a **flat list of permission slugs** for every component the package registers. The list only declares what the package needs. The permissions take effect after the person running the daemon adds `[package.acme] trusted = true` to `grants.toml`.

::: warning
Declare only what your package needs. People read this list before they trust a package, and a long list makes them less likely to.
:::

## Entry point (`main.lua`)

The entry point registers components with the usual `agentd.*` API. Everything registered here gets the package name as a prefix when it loads.

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

## Prefixing rules

| What you write | What gets registered |
|---|---|
| `agentd.tool{ name = "git" }` | `acme/git` |
| `agentd.action("git.diff", …)` | `acme/git.diff` |
| `agentd.runner{ name = "reviewer" }` | `acme/reviewer` |
| `actions = { "git.diff" }` inside a runner | resolved to `acme/git.diff` |

Callers use the full name: `agentctl call acme/git.diff`.

## See also

- [Managing packages](/v0/packages/managing)
- [Packages overview](/v0/packages/)
- [grants.toml reference](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)

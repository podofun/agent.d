# Packages

A **package** is a bundle of agent.d components (tools, actions, runners, services, and skills) that someone else wrote, together with the list of permissions they need. You install packages from git repositories and approve their permissions in `grants.toml`.

## What a package is

A package lives in its own directory and has a `package.toml` manifest that declares:

- Its name, version, and entry point.
- The **complete set of permissions** every component in the package may need.

When the package is loaded, every component it registers gets the package name as a **prefix**. A tool named `git` inside the `acme` package becomes `acme/git`, an action `git.diff` becomes `acme/git.diff`, and a runner `reviewer` becomes `acme/reviewer`. Package components and your own tools can then share names without clashing.

## Installing grants nothing

Installing a package makes its code available with **no permissions**. agent.d shows you the permissions the package declares when you install it, so you can review them. They take effect once you approve the package in `grants.toml`.

## Approving a package

Add a single entry to `grants.toml` to approve the package's entire declared permission set:

```toml
[package.acme]
trusted = true
```

Every `acme/...` component then gets the permissions listed in `acme`'s `package.toml`. Without this entry, the package is installed but cannot use anything that needs a grant.

::: warning
Review the package's declared `permissions` before setting `trusted = true`. You are approving the full listed set for all components the package registers.
:::

To trust the package as a whole but narrow one component, give that component its own entry. A component's own entry takes priority over the package grant:

```toml
[package.acme]
trusted = true

[tool."acme/git"]
granted = ["shell.exec:git"]   # narrower than what acme declares
```

## Install location

Packages are stored under `$XDG_DATA_HOME/agentd/packages/`. An `index.toml` in that directory tracks installed packages.

## See also

- [Managing packages](/v0/packages/managing)
- [Authoring packages](/v0/packages/authoring)
- [grants.toml reference](/v0/security/grants)

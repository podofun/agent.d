# Packages

A **package** is a bundle of agent.d components — tools, actions, runners, services, and skills — with a declared permission set. You install packages from git repositories and approve their permissions in one place.

## What a package is

A package lives in its own directory and has a `package.toml` manifest that declares:

- Its name, version, and entry point.
- The **complete set of permissions** every component in the package may need.

When the package is loaded, every component it registers is **auto-prefixed** with the package name. A tool named `git` inside the `acme` package becomes `acme/git`; an action `git.diff` becomes `acme/git.diff`; a runner `reviewer` becomes `acme/reviewer`. This namespace separation means package components never collide with your own tools.

## Installing does not trust

Installing a package makes its code available but grants it **zero permissions**. The package's declared permission set is shown to you at install time so you can review it, but nothing is granted until you explicitly approve it in `grants.toml`.

## Approving a package

Add a single entry to `grants.toml` to approve the package's entire declared permission set:

```toml
[package.acme]
trusted = true
```

This causes every `acme/...` component to inherit the permissions listed in `acme`'s `package.toml`. Without this entry the package is installed but inert.

::: warning
Review the package's declared `permissions` before setting `trusted = true`. You are approving the full listed set for all components the package registers.
:::

If you want to allow the package in general but restrict one specific component, add an explicit entry — explicit entries always override the inherited package grant:

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

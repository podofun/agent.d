# agentd-permissions

Auth — enforcement. Default-deny.

5-layer intersection engine:

```
tool-pkg ∩ action-requires ∩ runner-allow ∩ interface-allow ∩ policy = Decision
```

- Loads `grants.toml` — the **only** source of grants. A tool manifest's `requires` is a wishlist, never self-granting.
- Emits `Decision::Allow` / `NeedsConfirmation` / `Deny`.
- `Decision::is_escalatable()` flags Tool-missing + confirm (eligible for interactive approval).
- `[policy] auto_confirm` promotes a confirm action to Allow.

Permission slug shape: `domain[:specifier]`, with wildcards on the specifier (`net:*`, `fs.read:/tmp/**`).

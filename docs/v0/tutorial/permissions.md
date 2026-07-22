# Step 3: Give Permissions

agent.d denies access to a host capability when the necessary grant is absent. You must give each tool and runner its permissions.

## Write `grants.toml`

Create `~/.config/agentd/grants.toml` with this content:

```toml
[tool.git]
granted = [
    "shell.exec:git",
    "fs.read:/path/to/repository/**",
]

[runner.backend_reviewer]
allowed_actions = ["git.diff"]
granted = ["ai:anthropic-cli"]
```

Replace `/path/to/repository` with the same absolute path that you used in `tools/git.lua`.

The `[tool.git]` section permits the `git` tool to run Git and read the selected repository.

The `shell.exec:git` slug limits process execution to Git. The `fs.read` slug limits file access to the selected repository.

The `[runner.backend_reviewer]` section gives two limits:

- `allowed_actions` permits the runner to call only `git.diff`.
- `granted` permits the runner to use the `anthropic-cli` provider.

The provider grant does not give access to other actions. The action list does not give access to other providers.

## Default-deny behavior

The `requires` field in Lua declares a permission. The applicable section in `grants.toml` gives that permission.

If you remove `[tool.git]`, the runtime denies the Git process. If you remove `fs.read`, Git cannot read the repository.

If you remove the runner grant, the runtime denies the model call.

If you remove `git.diff` from `allowed_actions`, the runner cannot call that action.

This separation prevents a component from receiving a capability only because another component has it.

## Next step

[Step 4: Add a runner and a skill](/v0/tutorial/runner-and-skill)

## See also

- [Permission concepts](/v0/concepts/permissions)
- [Grant reference](/v0/security/grants)
- [Permission slugs](/v0/security/permission-slugs)

# Step 2: Write the Git Action

A tool contains related actions. An action is a function that a caller can use.

In this step, you will make one action. The action runs `git diff --staged` in your repository directory.

## Write `tools/git.lua`

Create `~/.config/agentd/tools/git.lua` with this content:

```lua
local repository = "/path/to/repository"

agentd.tool({
    name = "git",
    requires = {
        "shell.exec:git",
        "fs.read:" .. repository .. "/**",
    },
})

agentd.action({
    name = "git.diff",
    requires = {
        "shell.exec:git",
        "fs.read:" .. repository .. "/**",
    },
    handler = function(_, ctx)
        local result = ctx.shell("git", {
            "diff",
            "--staged",
        }, {
            cwd = repository,
            separate_stderr = false,
        })

        return {
            exit_code = result.exit_code,
            diff = result.stdout,
        }
    end,
})
```

Replace `/path/to/repository` with the absolute path of the repository that the agent will review.

## Understand the tool

`agentd.tool` registers the `git` tool. The `requires` field declares the two necessary host capabilities.

`agentd.action` registers the `git.diff` action. An action name has the form `tool.action`.

The handler receives the capability handle as `ctx`. The underscore shows that the handler does not use action arguments.

The `ctx.shell` function starts a process without a shell. Each array item is one separate command argument.

The action runs this command:

```text
git diff --staged
```

The `cwd = repository` setting runs Git in the repository directory.

The `separate_stderr = false` setting puts standard error in `result.stdout`. Thus, the caller receives Git error messages in `diff`.

The handler returns the Git exit code and the staged diff. agent.d serializes this table as JSON.

## Understand the permission declaration

The `shell.exec:git` slug permits the Git process. The `fs.read` slug permits read access only in the specified repository.

The `requires` field declares necessary permissions. It does not give the permissions.

The runtime denies the action until you add the grant in Step 3.

## Next step

[Step 3: Give permissions](/v0/tutorial/permissions)

## See also

- [Tool and action concepts](/v0/concepts/tools-and-actions)
- [ctx.shell reference](/v0/reference/ctx/shell)
- [Write tools](/v0/writing/tools)

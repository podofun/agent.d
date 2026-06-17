# Working Rules

Use these rules when changing this repository. Keep them short, practical, and current.

## Repository Approach

- Prefer the existing architecture and APIs over new abstractions.
- Read the nearby code before changing behavior. Let crate boundaries and tests show where a change belongs.
- Do not duplicate reference documentation here. Config defaults, command syntax, routes, and crate responsibilities should stay discoverable from code, README files, and tests.
- Keep changes scoped. Avoid opportunistic rewrites while fixing a bug or adding a feature.
- When behavior changes, update the user-facing docs or examples that exercise that behavior.
- Do not reference assistant-only instruction files from README files, crate docs, examples, comments, or user-facing documentation. They are instructions to follow, not product or project references.
- Do not hard-wrap prose or bullets. Keep each sentence or bullet on one line unless structure or readability genuinely requires a line break.

## Rust Guidance

- Keep enforcement, durable state, scheduling, transport, process execution, credential access, filesystem access, network access, tracing, and public API boundaries in Rust.
- Put logic in the crate that owns the concept. Avoid pushing policy into primitive crates that only perform an operation.
- Use typed structures and serde at boundaries instead of loosely shaped maps or string protocols.
- Preserve explicit permission checks around host capabilities. New privileged behavior needs tests for allowed and denied paths.
- Prefer small functions with clear errors. Use `anyhow::Context` at application edges and domain-specific errors inside reusable crates when that is already the local style.
- Do not shell out through string commands. Use argv-based process execution.
- Run focused tests for the crate you changed; broaden only when the change crosses crate or protocol boundaries.

## Lua Guidance

- Lua owns user-defined behavior: registrations, actions, runners, skills, services, event glue, and small workflow composition.
- Use LuaLS annotations for public examples and larger Lua modules: `---@param`, `---@return`, `---@class`, `---@field`, and `---@type` where they improve editor feedback.
- Use the sandbox-provided APIs before writing helpers from scratch. In particular, prefer built-ins such as `string.trim`, `string.startswith`, `string.endswith`, `string.contains`, and `string.split`.
- Respect the built-in concurrency model. Use the provided async, await, sleep, service, and channel APIs instead of inventing ad hoc schedulers or blocking loops.
- Keep action handlers small. Move reusable Lua behavior into local functions or modules loaded through `agentd.import`.
- For larger projects and examples, alias the API once:

```lua
local d = agentd

d.tool({ name = "git" })
d.action({
  name = "git.status",
  handler = function(_, ctx)
    return ctx.shell("git", { "status", "--short" })
  end,
})
```

- For small minimal examples, use plain `agentd.` calls so readers see the API name directly:

```lua
agentd.action("ping", function()
  return { ok = true }
end)
```

## Examples And Docs

- Examples should be runnable and minimal, but not toy-shaped when they are meant to teach a real workflow.
- Prefer showing current APIs over explaining historical behavior.
- Keep comments useful and sparse. Do not narrate obvious code.
- If an example needs permissions, make the required grant obvious near the example or in the paired grants file.
- Do not cite assistant-only workflow rules as documentation sources.

## Git Workflow

- Use a branch per feature or fix unless the user explicitly asks to work directly on `main`.
- Prefer micro-commits that separate meaningful steps over one large mixed commit.
- Commit messages should use a simple prefix: `feat:`, `fix:`, `chore:`, or `refactor:`.
- Never force-push.
- Use `git pull --rebase` when syncing is explicitly required or requested. Do not use merge pulls.
- Before committing, check `git status` and include only files relevant to the task.

## Pull Requests

- Use a simple Why / What / How structure.
- Why: the problem or motivation.
- What: the user-visible or maintainer-visible change.
- How: the implementation approach and any important validation.
- Keep PR text factual and focused on the change.

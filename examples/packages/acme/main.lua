-- Entry point for the `acme` package, loaded by `import("acme")`.
--
-- Everything registered here is auto-prefixed with the package name and
-- owner-tagged: tool `git` -> `acme/git`, action `git.diff` -> `acme/git.diff`,
-- runner `reviewer` -> `acme/reviewer`. Unqualified names in a runner's
-- `actions` list are rewritten to the qualified form (`acme/git.diff`).

agentd.tool{ name = "git", requires = { "shell.exec:git" } }

agentd.action("git.diff", function(args, ctx)
  local out = ctx.shell("git", { "diff" }, { cwd = args.cwd })
  return { diff = out.stdout }
end)

agentd.runner{
  name = "reviewer",
  model = "anthropic/claude-opus-4-7",
  -- short name; resolved to `acme/git.diff` at registration.
  actions = { "git.diff" },
}

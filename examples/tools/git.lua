-- Minimal git tool. Each action shells out to the `git` binary through
-- `ctx.shell`, gated by the scoped `shell.exec:git` permission declared below
-- least privilege: this tool can run git and nothing else.

agentd.tool({
	name = "git",
	requires = { "shell.exec:git" },
})

-- Run a git subcommand in `args.cwd` (default: current dir). `ctx` is the
-- per-invocation capability handle the runtime passes as the handler's
-- second argument
local function git(ctx, args, sub)
	args = args or {}
	local argv = { "-C", args.cwd or "." }
	for _, a in ipairs(sub) do
		table.insert(argv, a)
	end
	local res = ctx.shell("git", argv, { separate_stderr = false })
	return { exit_code = res.exit_code, output = res.stdout }
end

agentd.action({
	name = "git.diff",
	requires = { "shell.exec:git" },
	handler = function(args, ctx)
		args = args or {}
		local sub = { "diff" }
		if args.staged then
			table.insert(sub, "--staged")
		end
		ctx.log.info("git.diff cwd=" .. (args.cwd or "."))
		local r = git(ctx, args, sub)
		return { diff = r.output, exit_code = r.exit_code }
	end,
})

agentd.action({
	name = "git.status",
	requires = { "shell.exec:git" },
	handler = function(args, ctx)
		local r = git(ctx, args, { "status", "--porcelain=v1" })
		return { status = r.output, exit_code = r.exit_code }
	end,
})

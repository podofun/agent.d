-- Minimal userland for the container image. Mounted at /etc/agentd/init.lua.
-- Replace with your own tools, skills, and runners.
--
-- `import()` resolves relative to this file's directory, so the whole tree you
-- mount at /etc/agentd is available — split your userland across as many files
-- as you like.
local greet = import("lib/greet.lua")

-- Actions are what clients invoke: `agentctl call ping`.
agentd.action({
	name = "ping",
	handler = function()
		return "pong"
	end,
})

agentd.action({
	name = "greet",
	handler = function(args)
		args = args or {}
		return greet(args.name or "world")
	end,
})

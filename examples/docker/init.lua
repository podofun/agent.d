-- Minimal userland for the container image. Mounted at /etc/agentd/init.lua.
-- Replace with your own tools, skills, and runners.

agentd.tool({
	name = "ping",
	description = "Liveness check: returns pong.",
	handler = function()
		return "pong"
	end,
})

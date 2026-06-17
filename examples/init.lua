-- Single entry point. The daemon evaluates this file at startup; everything
-- else is pulled in via `import(...)` or the explicit skill loaders. The
-- layout under the root is up to you — this could just as well be one file.

-- `import` resolves paths relative to this file and refuses absolute paths
-- and `..` traversal.
import("tools/git.lua")

-- Markdown skills: walk a directory, or load one file at a time.
agentd.skills.dir("skills")

-- Inline-defined skill (alternative to a Markdown file).
agentd.skill({
	name = "terse",
	description = "no preamble, no markdown headers",
	system = "Reply in plain text. No preamble. No markdown headers.",
})

import("runners/backend_reviewer.lua")

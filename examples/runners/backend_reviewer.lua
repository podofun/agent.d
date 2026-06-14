-- Example runner. Composes the `reviewer` skill, picks the model via
-- "<provider>/<model_id>" — the prefix routes to a Provider impl in the
-- daemon's ProviderRegistry. The grants file is still authoritative for
-- the engine's layer-3 check — this action list is advisory.

agentd.runner({
	name = "backend_reviewer",
	system = "Reply in plain text. No markdown headers.",
	model = "anthropic/claude-opus-4-7",
	skills = { "reviewer" },
	actions = { "git.diff", "git.status" },
})

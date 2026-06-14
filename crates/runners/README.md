# agentd-runners

Runner — a named AI worker/identity.

- `RunnerDef { name, system, model, skills, allowed_actions }`. `model = "<provider>/<model_id>"`; the provider prefix resolves against the shared `ProviderRegistry`.
- `RunnerRegistry`.
- `compose()` — unions skill bodies + `system`, and skill `actions` + `allowed_actions`.
- `run()` — single-shot: composes the prompt, dispatches `Provider::complete`, returns text.

The multi-turn tool-use loop lives in `agentd-executor`, not here.

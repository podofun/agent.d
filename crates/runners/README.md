# agentd-runners

`agentd-runners` defines named model-runner configurations and executes them.

A `RunnerDef` selects a model, system instructions, skills, and allowed actions. `compose` resolves the referenced skills and produces the effective prompt and action list. `run` resolves the provider and returns a typed `RunnerOutcome`.

The registry stores runner definitions. The executor supplies tool dispatch and permission enforcement.

# agentd-executor

`agentd-executor` runs actions and runners through the agentd policy boundary.

For each action, the executor:

- Resolves action and tool metadata.
- Evaluates required permissions, caller allowlists, and global policy.
- Requests operator approval when the denial is eligible for escalation.
- Invokes the registry with the effective grants and call context.
- Records a redacted execution trace.

It also composes runner skills, drives executor-owned model tool loops, and exposes dispatch bridges for MCP, Lua, and provider-owned loops.

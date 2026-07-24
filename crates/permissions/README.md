# agentd-permissions

`agentd-permissions` defines permission values, grant files, caller identity, and policy evaluation.

Permissions use scoped strings such as filesystem or network grants. The engine checks required permissions, runner, interface, and service allowlists, global deny rules, and confirmation settings. It returns an allow, deny, or confirmation decision and identifies the layer that caused a denial.

This crate evaluates policy. It does not perform host operations or operator approval transport.

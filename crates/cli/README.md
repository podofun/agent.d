# agentd-cli

`agentd-cli` builds the `agentctl` command-line client.

The client can:

- Check daemon health and list or call actions.
- List, inspect, and run runners and skills.
- List services and read execution traces.
- Listen for approval requests on the control WebSocket.
- Manage local secrets and packages.
- Generate Lua type definitions.

Remote commands use the daemon WebSocket API. Local management commands read or update files on the host.

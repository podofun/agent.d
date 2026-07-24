# agentd-mcp

`agentd-mcp` exposes registered agentd actions as Model Context Protocol tools.

The loopback server:

- Binds to a local TCP address.
- Requires a generated bearer token.
- Implements MCP initialization, tool listing, and tool calls.
- Routes calls through the shared `Dispatcher`.
- Stops when its handle is shut down or dropped.

The server is a loopback bridge for provider-owned model loops. It does not bypass executor permissions.

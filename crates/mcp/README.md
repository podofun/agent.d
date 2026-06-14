# agentd-mcp

MCP loopback. Lets `ProviderOwned` CLI providers (claude, codex) reach back into the
executor without a circular crate dep.

`bind_loopback(dispatcher, caller, tools)` spawns a per-invocation HTTP JSON-RPC
server on `127.0.0.1:0` exposing the given catalog as MCP tools. Every `tools/call`
runs through the supplied `agentd_types::Dispatcher`, so the 5-layer permission engine
fires — same path as a user-initiated `actions.call`.

The executor binds it for the duration of a runner call and tears it down on return.

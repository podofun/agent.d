# agentd-codex

Codex transport. Long-lived `codex app-server` subprocess client.

Newline-delimited JSON-RPC over stdio, bidirectional — server-issued requests come back through the inbox. Hand-coded subset of the codex protocol covering just the methods agentd uses.

Consumed by `agentd-ai`'s `CodexAppServerProvider`.

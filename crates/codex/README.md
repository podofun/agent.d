# agentd-codex

`agentd-codex` is an asynchronous client for `codex app-server`.

It starts the app server as a child process, exchanges JSON-RPC messages over standard input and output, and separates responses, notifications, and server requests. The `protocol` module contains the typed request and response structures used to initialize threads and run turns.

This crate implements the transport client. `agentd-ai` adapts it to the model-provider interface and applies agentd permission decisions.

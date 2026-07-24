# agentd-net

`agentd-net` provides network clients used by the Lua runtime and model providers.

Modules:

- `http` sends typed HTTP requests and returns a status, headers, and a text body.
- `ws` opens WebSocket connections and reads or writes typed frames.
- `mailer` sends messages and attachments through SMTP.

The crate derives target hosts from URLs for permission checks. Callers must enforce the required network permissions before they connect.

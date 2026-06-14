# agentd-ws

WebSocket client primitive. Built on `tokio-tungstenite` + rustls.

`Connection::connect(url)` → `send_text` / `send_binary` / `recv(timeout?)` / `close`
/ `is_closed`. `Frame::{Text, Binary, Close}`.

`host_of(url)` helper derives the `net:<host>` slug. Permission gating lives in the
caller (scripting `ctx.ws`).

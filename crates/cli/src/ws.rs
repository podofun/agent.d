//! WebSocket transport to the public `/ws` plane. One connection per call:
//! open, send a single JSON envelope, read the response, close.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

#[derive(Serialize)]
struct WsRequest<'a> {
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    params: Value,
}

#[derive(Deserialize, Debug)]
pub(crate) struct WsResponse {
    #[allow(dead_code)]
    pub(crate) id: u64,
    pub(crate) ok: bool,
    #[serde(default)]
    pub(crate) result: Option<Value>,
    #[serde(default)]
    pub(crate) error: Option<String>,
    #[serde(default)]
    pub(crate) code: Option<String>,
    #[serde(default)]
    pub(crate) tip: Option<String>,
    #[serde(default)]
    pub(crate) trace: Option<Vec<String>>,
}

/// Monotonic envelope ids, shared across the public and control planes.
pub(crate) static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn ws_url_of(base: &str) -> Result<String> {
    ws_url_with_path(base, "/ws")
}

/// Turn an http(s)/ws(s) base URL into a WebSocket URL at `path`.
pub(crate) fn ws_url_with_path(base: &str, path: &str) -> Result<String> {
    let mut u = url::Url::parse(base).context("the --url value is not a valid URL")?;
    let scheme = match u.scheme() {
        "http" => "ws".to_string(),
        "https" => "wss".to_string(),
        "ws" | "wss" => u.scheme().to_string(),
        other => {
            return Err(anyhow!(
                "--url must start with http, https, ws, or wss (got `{other}`)"
            ));
        }
    };
    u.set_scheme(&scheme)
        .map_err(|_| anyhow!("could not build a WebSocket URL from --url"))?;
    u.set_path(path);
    Ok(u.to_string())
}

/// Bearer token for the `/ws` handshake: `AGENTD_TOKEN` wins, else the token
/// the daemon persisted to its state dir. `None` when neither exists (the
/// daemon may be running with `--no-auth`).
pub(crate) fn resolve_ws_token() -> Option<String> {
    token_from_env_or_file("AGENTD_TOKEN", "token")
}

/// A bearer token from `$var`, falling back to `<state-dir>/agentd/<file>`.
/// Blank values (env or file) are treated as absent.
pub(crate) fn token_from_env_or_file(var: &str, file: &str) -> Option<String> {
    if let Ok(t) = std::env::var(var) {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let path = dirs::state_dir()
        .or_else(dirs::data_local_dir)?
        .join("agentd")
        .join(file);
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(crate) async fn ws_call(
    base: &str,
    timeout: u64,
    method: &str,
    params: Value,
) -> Result<WsResponse> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = ws_url_of(base)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("could not build a request for `{url}`"))?;
    if let Some(token) = resolve_ws_token() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .context("the auth token contains characters that cannot go in a header")?,
        );
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (mut ws, _) = tokio::time::timeout(Duration::from_millis(timeout), connect)
        .await
        .with_context(|| format!("timed out connecting to `{url}`"))?
        .with_context(|| format!("could not connect to `{url}` — is agentd running?"))?;

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let req = WsRequest { id, method, params };
    let body = serde_json::to_string(&req)?;
    ws.send(Message::Text(body.into())).await?;

    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(t) => {
                let resp: WsResponse = serde_json::from_str(&t).with_context(|| {
                    format!("the daemon sent a response that could not be decoded ({t})")
                })?;
                let _ = ws.send(Message::Close(None)).await;
                return Ok(resp);
            }
            Message::Binary(b) => {
                let resp: WsResponse = serde_json::from_slice(&b)?;
                let _ = ws.send(Message::Close(None)).await;
                return Ok(resp);
            }
            Message::Close(_) => break,
            _ => continue,
        }
    }
    Err(anyhow!("ws closed before response"))
}

/// Like [`ws_call`], but relays `runner.delta` event frames to `on_delta` as
/// they arrive and returns the final complete response envelope. Only the
/// connect handshake is bounded by `timeout` — a streaming run legitimately
/// outlives any fixed request budget.
pub(crate) async fn ws_call_streaming(
    base: &str,
    timeout: u64,
    method: &str,
    params: Value,
    on_delta: impl FnMut(&Value),
) -> Result<WsResponse> {
    ws_call_streaming_cancelable(base, timeout, method, params, None, on_delta).await
}

pub(crate) async fn ws_call_streaming_cancelable(
    base: &str,
    timeout: u64,
    method: &str,
    params: Value,
    mut cancel: Option<tokio::sync::oneshot::Receiver<()>>,
    mut on_delta: impl FnMut(&Value),
) -> Result<WsResponse> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = ws_url_of(base)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("could not build a request for `{url}`"))?;
    if let Some(token) = resolve_ws_token() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .context("the auth token contains characters that cannot go in a header")?,
        );
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (mut ws, _) = tokio::time::timeout(Duration::from_millis(timeout), connect)
        .await
        .with_context(|| format!("timed out connecting to `{url}`"))?
        .with_context(|| format!("could not connect to `{url}` — is agentd running?"))?;

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let req = WsRequest { id, method, params };
    let body = serde_json::to_string(&req)?;
    ws.send(Message::Text(body.into())).await?;

    loop {
        let msg = if let Some(receiver) = cancel.as_mut() {
            tokio::select! {
                msg = ws.next() => msg,
                _ = receiver => {
                    cancel = None;
                    let cancel_id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                    let request = WsRequest {
                        id: cancel_id,
                        method: "runners.cancel",
                        params: serde_json::json!({ "id": id }),
                    };
                    ws.send(Message::Text(serde_json::to_string(&request)?.into())).await?;
                    continue;
                }
            }
        } else {
            ws.next().await
        };
        let Some(msg) = msg else {
            break;
        };
        let text = match msg? {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Close(_) => break,
            _ => continue,
        };
        // Event frames carry `event`; the final envelope does not.
        if let Ok(v) = serde_json::from_str::<Value>(&text)
            && v.get("event").and_then(|e| e.as_str()) == Some("runner.delta")
        {
            if let Some(d) = v.get("delta") {
                on_delta(d);
            }
            continue;
        }
        let resp: WsResponse = serde_json::from_str(&text).with_context(|| {
            format!("the daemon sent a response that could not be decoded ({text})")
        })?;
        if resp.id != id {
            continue;
        }
        let _ = ws.send(Message::Close(None)).await;
        return Ok(resp);
    }
    Err(anyhow!("ws closed before response"))
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;

    use super::ws_call_streaming_cancelable;

    #[tokio::test]
    async fn cancellation_uses_the_run_connection_and_waits_for_its_final_response() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("could not bind test socket: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let Message::Text(run) = socket.next().await.unwrap().unwrap() else {
                panic!("run request expected")
            };
            let run: Value = serde_json::from_str(&run).unwrap();
            assert_eq!(run["method"], "runners.run");
            started_tx.send(()).unwrap();
            let Message::Text(cancel) = socket.next().await.unwrap().unwrap() else {
                panic!("cancel request expected")
            };
            let cancel: Value = serde_json::from_str(&cancel).unwrap();
            assert_eq!(cancel["method"], "runners.cancel");
            assert_eq!(cancel["params"]["id"], run["id"]);
            socket
                .send(Message::Text(
                    json!({ "id": cancel["id"], "ok": true, "result": { "cancelled": true } })
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            socket.send(Message::Text(json!({ "id": run["id"], "ok": false, "code": "cancelled", "error": "runner cancelled" }).to_string().into())).await.unwrap();
        });
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let client = tokio::spawn(async move {
            ws_call_streaming_cancelable(
                &format!("ws://{address}"),
                1000,
                "runners.run",
                json!({ "name": "test", "prompt": "go" }),
                Some(cancel_rx),
                |_| {},
            )
            .await
            .unwrap()
        });
        started_rx.await.unwrap();
        cancel_tx.send(()).unwrap();
        let response = client.await.unwrap();
        assert_eq!(response.code.as_deref(), Some("cancelled"));
        server.await.unwrap();
    }
}

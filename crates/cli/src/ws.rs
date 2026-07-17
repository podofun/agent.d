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

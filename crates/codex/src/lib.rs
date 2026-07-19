//! Codex app-server client.
//!
//! Spawns `codex app-server --listen stdio://` as a long-lived subprocess
//! and speaks the JSON-RPC protocol over its stdin/stdout.
//!
//! This crate is the transport + protocol layer only. Mapping codex
//! approval requests onto agentd grant slugs lives upstream in
//! `ai::CodexAppServerProvider`, which consumes [`Inbound`] events
//! and replies via [`Client::reply`].

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

pub mod protocol;
pub use protocol::*;

#[derive(Debug, Error)]
pub enum Error {
    #[error("spawn codex: {0}")]
    Spawn(std::io::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("transport: {0}")]
    Transport(String),
    #[error("rpc: {code} {message}")]
    Rpc { code: i64, message: String },
    #[error("response missing or malformed for id {0}")]
    BadResponse(i64),
}

pub type Result<T> = std::result::Result<T, Error>;

/// One message off the wire. Notifications and server-requests reach the
/// caller via the inbox; responses to our own requests are routed via the
/// internal pending map and never surface as `Inbound`.
#[derive(Debug, Clone)]
pub enum Inbound {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
}

#[derive(Debug, Clone)]
enum ResponseBody {
    Ok(Value),
    Err { code: i64, message: String },
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<ResponseBody>>>>;

/// Long-lived handle.
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    next_id: AtomicI64,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    bin: String,
    _reader_task: Mutex<Option<JoinHandle<()>>>,
    child: Mutex<Option<Child>>,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        // tokio's `Child` does NOT kill the process on drop, so the long-lived
        // `codex app-server` would outlive the provider and leak. `shutdown()`
        // is the graceful path; this is the safety net when nobody calls it.
        // `get_mut` is contention-free here — Drop holds the only reference.
        if let Some(mut child) = self.child.get_mut().take() {
            let _ = child.start_kill();
        }
        if let Some(task) = self._reader_task.get_mut().take() {
            task.abort();
        }
    }
}

impl Client {
    /// Spawn `codex app-server` and return a handle plus the inbox
    /// channel for notifications + server requests.
    pub async fn spawn(bin: impl Into<String>) -> Result<(Self, mpsc::UnboundedReceiver<Inbound>)> {
        Self::spawn_with_env(bin, &[]).await
    }

    /// Like [`Client::spawn`] but seeds extra environment variables on the
    /// child. Used to hand the MCP loopback's bearer token to codex via the
    /// env var its `mcp_servers.<name>.bearer_token_env_var` points at.
    pub async fn spawn_with_env(
        bin: impl Into<String>,
        env: &[(String, String)],
    ) -> Result<(Self, mpsc::UnboundedReceiver<Inbound>)> {
        let bin = bin.into();
        let mut cmd = agentd_process::command(&bin);
        cmd.arg("app-server")
            .arg("--listen")
            .arg("stdio://")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(Error::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Transport("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Transport("no stdout".into()))?;

        let (inbox_tx, inbox_rx) = mpsc::unbounded_channel::<Inbound>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let r_pending = pending.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        if let Err(e) = handle_line(&line, &r_pending, &inbox_tx).await {
                            tracing::warn!(error = %e, line = %line, "codex frame dropped");
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "codex stdout read error");
                        break;
                    }
                }
            }
        });

        let inner = Arc::new(ClientInner {
            next_id: AtomicI64::new(1),
            stdin: Mutex::new(Some(stdin)),
            pending,
            bin,
            _reader_task: Mutex::new(Some(reader)),
            child: Mutex::new(Some(child)),
        });
        Ok((Client { inner }, inbox_rx))
    }

    pub fn bin(&self) -> &str {
        &self.inner.bin
    }

    /// Sends a JSON-RPC request.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().await.insert(id, tx);
        let frame = serde_json::to_string(&JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        })?;
        self.write_line(&frame).await?;
        match rx.await {
            Ok(ResponseBody::Ok(v)) => Ok(v),
            Ok(ResponseBody::Err { code, message }) => Err(Error::Rpc { code, message }),
            Err(_) => Err(Error::BadResponse(id)),
        }
    }

    /// Reply to a server-issued request.
    pub async fn reply(&self, id: Value, result: Value) -> Result<()> {
        let frame = serde_json::to_string(&JsonRpcResponse {
            jsonrpc: "2.0",
            id,
            result,
        })?;
        self.write_line(&frame).await
    }

    pub async fn reply_err(&self, id: Value, code: i64, message: impl Into<String>) -> Result<()> {
        let frame = serde_json::to_string(&JsonRpcErrorResp {
            jsonrpc: "2.0",
            id,
            error: JsonRpcErrorBody {
                code,
                message: message.into(),
            },
        })?;
        self.write_line(&frame).await
    }

    pub async fn shutdown(self) -> Result<()> {
        // Drop stdin to signal EOF.
        {
            let mut slot = self.inner.stdin.lock().await;
            *slot = None;
        }
        let mut child_slot = self.inner.child.lock().await;
        if let Some(mut child) = child_slot.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
            let _ = child.start_kill();
        }
        Ok(())
    }

    async fn write_line(&self, frame: &str) -> Result<()> {
        let mut slot = self.inner.stdin.lock().await;
        let stdin = slot
            .as_mut()
            .ok_or_else(|| Error::Transport("stdin closed".into()))?;
        stdin.write_all(frame.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }
}

async fn handle_line(
    line: &str,
    pending: &Pending,
    inbox: &mpsc::UnboundedSender<Inbound>,
) -> Result<()> {
    let v: Value = serde_json::from_str(line)?;
    let has_method = v.get("method").is_some();
    let has_id = v.get("id").is_some();
    if has_id && !has_method {
        // Response.
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let body = if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000);
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown")
                .to_string();
            ResponseBody::Err { code, message }
        } else {
            ResponseBody::Ok(v.get("result").cloned().unwrap_or(Value::Null))
        };
        if let Some(id_i) = id.as_i64()
            && let Some(tx) = pending.lock().await.remove(&id_i)
        {
            let _ = tx.send(body);
            return Ok(());
        }
        // Unmatched response — log + drop. Codex sometimes echoes
        // responses to itself; surfacing as transport noise isn't useful.
        tracing::debug!(?id, "codex: unmatched response");
        return Ok(());
    }
    if !has_method {
        return Err(Error::Transport(format!("unrecognized frame: {line}")));
    }
    let method = v
        .get("method")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    if has_id {
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let _ = inbox.send(Inbound::ServerRequest { id, method, params });
    } else {
        let _ = inbox.send(Inbound::Notification { method, params });
    }
    Ok(())
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'static str,
    id: i64,
    method: &'a str,
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    result: Value,
}

#[derive(Serialize)]
struct JsonRpcErrorResp {
    jsonrpc: &'static str,
    id: Value,
    error: JsonRpcErrorBody,
}

#[derive(Serialize, Deserialize)]
struct JsonRpcErrorBody {
    code: i64,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_map() -> Pending {
        Arc::new(Mutex::new(HashMap::new()))
    }

    #[tokio::test]
    async fn handle_line_routes_ok_response_to_pending() {
        let pending = pending_map();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(42, tx);
        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        handle_line(
            r#"{"id":42,"result":{"hello":"world"}}"#,
            &pending,
            &inbox_tx,
        )
        .await
        .unwrap();
        match rx.await.unwrap() {
            ResponseBody::Ok(v) => assert_eq!(v["hello"], "world"),
            other => panic!("expected Ok, got {other:?}"),
        }
        assert!(inbox_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn handle_line_routes_error_response() {
        let pending = pending_map();
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(7, tx);
        let (inbox_tx, _) = mpsc::unbounded_channel();
        handle_line(
            r#"{"id":7,"error":{"code":-32601,"message":"nope"}}"#,
            &pending,
            &inbox_tx,
        )
        .await
        .unwrap();
        match rx.await.unwrap() {
            ResponseBody::Err { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_line_surfaces_notification() {
        let pending = pending_map();
        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        handle_line(
            r#"{"method":"thread/started","params":{"thread":{"id":"x"}}}"#,
            &pending,
            &inbox_tx,
        )
        .await
        .unwrap();
        match inbox_rx.recv().await.unwrap() {
            Inbound::Notification { method, params } => {
                assert_eq!(method, "thread/started");
                assert_eq!(params["thread"]["id"], "x");
            }
            other => panic!("expected Notification, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn handle_line_surfaces_server_request() {
        let pending = pending_map();
        let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel();
        handle_line(
            r#"{"id":"abc","method":"item/commandExecution/requestApproval","params":{"command":"ls"}}"#,
            &pending,
            &inbox_tx,
        )
        .await
        .unwrap();
        match inbox_rx.recv().await.unwrap() {
            Inbound::ServerRequest { id, method, params } => {
                assert_eq!(id, serde_json::json!("abc"));
                assert_eq!(method, "item/commandExecution/requestApproval");
                assert_eq!(params["command"], "ls");
            }
            other => panic!("expected ServerRequest, got {other:?}"),
        }
    }
}

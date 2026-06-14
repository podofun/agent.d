//! Loopback MCP server.
//!
//! Exposes a runner's `allowed_actions` as MCP tools so a `ProviderOwned`
//! provider (today: `ClaudeCliProvider`, tomorrow: `CodexCliProvider`) can
//! drive the agent loop inside the upstream CLI while still reaching back
//! into the agentd executor for each tool call. The permission engine runs
//! on every dispatched tool because the call goes through
//! `Executor::run`, same path as any other action.
//!
//! Transport: plain HTTP JSON-RPC. claude CLI's `--mcp-config` accepts
//! `"type": "http"` servers — one POST per request, JSON-RPC body in, JSON-RPC
//! body out.
//!
//! Lifecycle: each `run_runner` call w/ a ProviderOwned provider binds a
//! fresh listener on `127.0.0.1:0`, hands the URL into the provider via
//! `McpEndpoint::Http`, and drops the handle when complete. Tearing down
//! the handle aborts the server task — there is no long-lived sharing of
//! MCP state across runner invocations.

use std::net::SocketAddr;
use std::sync::Arc;

use agentd_ai::ToolDef;
use agentd_permissions::Caller;
use agentd_types::{ActionCall, Dispatcher};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "agentd";
const SERVER_VERSION: &str = "0.1.0";

/// Handle to a running loopback. Dropping the handle aborts the underlying
/// task and the listener closes immediately. Holding the handle keeps the
/// server alive for the duration of a runner invocation.
pub struct LoopbackHandle {
    pub url: String,
    pub local_addr: SocketAddr,
    /// Bearer token the provider must echo on every request. Even though the
    /// listener is on localhost and lives only for one invocation, any local
    /// process that guessed the ephemeral port could otherwise drive tool
    /// calls; the token shuts that door.
    pub token: String,
    task: Option<JoinHandle<()>>,
}

impl LoopbackHandle {
    /// Stop the loopback explicitly + wait for the task to fully drop.
    /// Useful in tests that need to free the port deterministically.
    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for LoopbackHandle {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
struct AppState {
    dispatcher: Arc<dyn Dispatcher>,
    caller: Caller,
    tools: Arc<Vec<ToolDef>>,
    token: Arc<String>,
}

/// 256 bits of OS randomness, hex-encoded. Unguessable per invocation.
pub fn gen_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS RNG unavailable");
    let mut s = String::with_capacity(64);
    for b in buf {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Bind a loopback MCP server on `127.0.0.1:0` and return a handle. Tools
/// are baked in at bind time — they don't change for the lifetime of the
/// handle, so the model sees a stable catalog for one runner invocation.
pub async fn bind_loopback(
    dispatcher: Arc<dyn Dispatcher>,
    caller: Caller,
    tools: Vec<ToolDef>,
    token: String,
) -> std::io::Result<LoopbackHandle> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    let state = AppState {
        dispatcher,
        caller,
        tools: Arc::new(tools),
        token: Arc::new(token.clone()),
    };
    let router: Router = Router::new()
        .route("/", post(handle_jsonrpc))
        .route("/mcp", post(handle_jsonrpc))
        .with_state(state);
    let url = format!("http://{}/mcp", local_addr);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::warn!(error = %e, "mcp loopback server exited");
        }
    });
    Ok(LoopbackHandle {
        url,
        local_addr,
        token,
        task: Some(task),
    })
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRpcOk {
    jsonrpc: &'static str,
    id: Value,
    result: Value,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRpcErr {
    jsonrpc: &'static str,
    id: Value,
    error: JsonRpcErrBody,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRpcErrBody {
    code: i32,
    message: String,
}

fn ok(id: Value, result: Value) -> Json<Value> {
    Json(
        serde_json::to_value(JsonRpcOk {
            jsonrpc: "2.0",
            id,
            result,
        })
        .unwrap(),
    )
}

fn err(id: Value, code: i32, message: impl Into<String>) -> Json<Value> {
    Json(
        serde_json::to_value(JsonRpcErr {
            jsonrpc: "2.0",
            id,
            error: JsonRpcErrBody {
                code,
                message: message.into(),
            },
        })
        .unwrap(),
    )
}

async fn handle_jsonrpc(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    // Gate every request on the bearer token. The header extractor runs before
    // `Json` (which consumes the body), so an unauthorized caller never reaches
    // the dispatcher. Reject with 401 and no JSON-RPC envelope.
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if presented != Some(state.token.as_str()) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }

    let _ = &req.jsonrpc; // We accept any version; clients in the wild send "2.0".
    let id = req.id.clone().unwrap_or(Value::Null);

    // Notifications carry no `id`. Per JSON-RPC the server MUST NOT reply
    // with a result/error envelope. The MCP "Streamable HTTP" transport
    // (rmcp client used by codex) is strict: returning `{}` makes the
    // client try to deserialize it as a JsonRpcMessage and the handshake
    // explodes with "data did not match any variant". 202 Accepted + empty
    // body is the conformant response, and claude CLI accepts it too.
    let is_notification = req.id.is_none();

    let response = match req.method.as_str() {
        "initialize" => ok(
            id.clone(),
            serde_json::json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            }),
        ),
        "notifications/initialized" | "notifications/cancelled" => {
            return (StatusCode::ACCEPTED, ()).into_response();
        }
        "ping" => ok(id, Value::Object(Default::default())),
        "tools/list" => {
            let tools: Vec<Value> = state
                .tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description.clone().unwrap_or_default(),
                        "inputSchema": if t.input_schema.is_null() {
                            serde_json::json!({ "type": "object" })
                        } else {
                            t.input_schema.clone()
                        },
                    })
                })
                .collect();
            ok(id, serde_json::json!({ "tools": tools }))
        }
        "tools/call" => handle_tools_call(state, id.clone(), req.params).await,
        other => err(id, -32601, format!("method `{other}` not implemented")),
    };

    if is_notification {
        (StatusCode::ACCEPTED, ()).into_response()
    } else {
        response.into_response()
    }
}

async fn handle_tools_call(state: AppState, id: Value, params: Value) -> Json<Value> {
    let name = match params.get("name").and_then(|v| v.as_str()) {
        Some(n) => n.to_string(),
        None => return err(id, -32602, "tools/call: missing `name`"),
    };
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    // Refuse calls for tools not in the runner's catalog. The executor's
    // permission engine would catch this too, but failing here gives a
    // tighter error and avoids leaking which actions exist outside the
    // exposed set.
    if !state.tools.iter().any(|t| t.name == name) {
        return tool_result_text(
            id,
            true,
            format!("tool `{name}` not in this runner's catalog"),
        );
    }

    let call = ActionCall {
        action: name.clone(),
        args: arguments,
    };
    match state.dispatcher.dispatch(state.caller.clone(), call).await {
        Ok((res, _dur)) => {
            let body = serde_json::to_string(&res.value)
                .unwrap_or_else(|e| format!("<serialize error: {e}>"));
            tool_result_text(id, false, body)
        }
        Err((e, _dur)) => tool_result_text(id, true, e.to_string()),
    }
}

/// Build the MCP `tools/call` success envelope. `is_error = true` flips the
/// `isError` flag so the upstream model knows to surface it back to the
/// user instead of treating it as data.
fn tool_result_text(id: Value, is_error: bool, text: impl Into<String>) -> Json<Value> {
    let body = serde_json::json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": is_error,
    });
    ok(id, body)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agentd_permissions::Caller;
    use agentd_types::{ActionResult, RegistryError};
    use async_trait::async_trait;

    use super::*;

    struct EchoDispatcher {
        calls: Mutex<Vec<ActionCall>>,
        deny_action: Option<String>,
    }

    #[async_trait]
    impl Dispatcher for EchoDispatcher {
        async fn dispatch(
            &self,
            _caller: Caller,
            call: ActionCall,
        ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
            self.calls.lock().unwrap().push(call.clone());
            if let Some(deny) = &self.deny_action
                && &call.action == deny
            {
                return Err((
                    RegistryError::Denied {
                        layer: "Runner".into(),
                        reason: format!("runner not allowed to call `{}`", call.action),
                    },
                    0,
                ));
            }
            Ok((
                ActionResult {
                    value: serde_json::json!({ "shouted": call.args }),
                },
                0,
            ))
        }
    }

    fn dispatcher() -> Arc<EchoDispatcher> {
        Arc::new(EchoDispatcher {
            calls: Mutex::new(Vec::new()),
            deny_action: None,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_list_returns_baked_catalog() {
        let disp = dispatcher();
        let handle = bind_loopback(
            disp.clone() as Arc<dyn Dispatcher>,
            Caller::default(),
            vec![ToolDef {
                name: "echo.shout".into(),
                description: Some("shout it".into()),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            gen_token(),
        )
        .await
        .unwrap();
        let url = handle.url.clone();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        });
        let resp: Value = reqwest::Client::new()
            .post(&url)
            .bearer_auth(&handle.token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "echo.shout");
        assert_eq!(tools[0]["description"], "shout it");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_call_dispatches_through_executor() {
        let disp = dispatcher();
        let handle = bind_loopback(
            disp.clone() as Arc<dyn Dispatcher>,
            Caller::default(),
            vec![ToolDef {
                name: "echo.shout".into(),
                description: None,
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            gen_token(),
        )
        .await
        .unwrap();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "echo.shout", "arguments": { "msg": "hi" } },
        });
        let resp: Value = reqwest::Client::new()
            .post(&handle.url)
            .bearer_auth(&handle.token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["id"], 7);
        let content = &resp["result"]["content"][0]["text"];
        let parsed: Value = serde_json::from_str(content.as_str().unwrap()).unwrap();
        assert_eq!(parsed["shouted"]["msg"], "hi");
        assert_eq!(resp["result"]["isError"], false);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn tools_call_unknown_tool_rejected() {
        let disp = dispatcher();
        let handle = bind_loopback(
            disp as Arc<dyn Dispatcher>,
            Caller::default(),
            vec![],
            gen_token(),
        )
        .await
        .unwrap();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "nope.nope", "arguments": {} },
        });
        let resp: Value = reqwest::Client::new()
            .post(&handle.url)
            .bearer_auth(&handle.token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("not in this runner's catalog"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_handshake_returns_capabilities() {
        let disp = dispatcher();
        let handle = bind_loopback(
            disp as Arc<dyn Dispatcher>,
            Caller::default(),
            vec![],
            gen_token(),
        )
        .await
        .unwrap();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {},
        });
        let resp: Value = reqwest::Client::new()
            .post(&handle.url)
            .bearer_auth(&handle.token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], "agentd");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_without_valid_token_are_rejected() {
        let disp = dispatcher();
        let handle = bind_loopback(
            disp.clone() as Arc<dyn Dispatcher>,
            Caller::default(),
            vec![],
            gen_token(),
        )
        .await
        .unwrap();
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });

        // No Authorization header at all.
        let no_token = reqwest::Client::new()
            .post(&handle.url)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(no_token.status(), reqwest::StatusCode::UNAUTHORIZED);

        // Present but wrong token.
        let wrong = reqwest::Client::new()
            .post(&handle.url)
            .bearer_auth("not-the-token")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

        // The dispatcher was never reached.
        assert!(disp.calls.lock().unwrap().is_empty());
    }
}

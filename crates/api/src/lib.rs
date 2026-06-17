//! Daemon control plane. The transport is WebSocket-only JSON envelopes (plus
//! `/health` for liveness probes); every method goes through the single `/ws`
//! endpoint rather than its own HTTP route.
//!
//! Envelope shape:
//!
//! ```json
//! // client -> server
//! { "id": 1, "method": "actions.call", "params": { "name": "git.diff", "args": {} } }
//! // server -> client (success)
//! { "id": 1, "ok": true, "result": { ... } }
//! // server -> client (error)
//! { "id": 1, "ok": false, "code": "not_found", "error": "action `x` not registered" }
//! ```
//!
//! Methods implemented:
//!
//! | method            | params                                     | result                              |
//! |-------------------|--------------------------------------------|-------------------------------------|
//! | `health`          | none                                       | `"ok"`                              |
//! | `tools.list`      | none                                       | `[name, ...]`                       |
//! | `actions.call`    | `{ name, args, session?, user? }`          | `{ result, duration_ms }`           |
//! | `runners.list`    | none                                       | `[{name, model, skills, ...}]`      |
//! | `runners.inspect` | `{ name }`                                 | `RunnerComposition`                 |
//! | `runners.run`     | `{ name, prompt, session?, user? }`        | `RunnerOutcome`                     |
//! | `skills.list`     | none                                       | `[{name, description, actions}]`    |
//! | `skills.inspect`  | `{ name }`                                 | `SkillDef`                          |
//! | `services.list`   | none                                       | `[ServiceStatus]`                   |
//!
//! Caller identity: every connection gets a session id `ws-<n>`; `actions.call`
//! and `runners.run` accept optional `session` / `user` params so bridging
//! interfaces (Telegram, Discord, …) can carry their own identity space.
//! Lua handlers read it back via `ctx.caller`.

use agentd_executor::Executor;
use agentd_permissions::Caller;
use agentd_runners::{RunnerError, compose};
use agentd_types::{ActionCall, RegistryError};
pub use axum::serve;
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::{IntoResponse, Response},
    routing::{any, get},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone)]
pub struct AppState {
    /// Hot-swappable executor. `daemon --watch` rebuilds the Lua runtime and
    /// `store()`s a fresh executor here; in-flight requests keep the `Arc` they
    /// `load()`ed and drain on the old runtime. Without `--watch` the pointer
    /// never changes.
    pub executor: Arc<arc_swap::ArcSwap<Executor>>,
    /// Bearer token required on the public `/ws` handshake. `None` disables auth
    /// (the daemon's `--no-auth`); `/health` is always open for liveness probes.
    pub auth_token: Option<Arc<String>>,
    /// Bearer token required on the `/control` handshake. Distinct from
    /// `auth_token` so a public-token holder can never reach the control plane.
    /// `None` disables the control gate (`--no-auth`).
    pub admin_token: Option<Arc<String>>,
    /// Approval broker, shared with the executor. The control plane registers
    /// operator clients against it (`subscribe`) and relays their verdicts
    /// (`resolve`).
    pub broker: Arc<agentd_approvals::Broker>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ws", any(ws_upgrade))
        .route("/control", any(control_upgrade))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(token) = &state.auth_token {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if presented != Some(token.as_str()) {
            return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// `/control` handshake. Gated by the **admin** token (separate from the public
/// `auth_token`) so the control plane is unreachable with a consumer token.
async fn control_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(token) = &state.admin_token {
        let presented = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if presented != Some(token.as_str()) {
            return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
        }
    }
    ws.on_upgrade(move |socket| handle_control_socket(socket, state))
}

#[derive(Deserialize)]
struct WsRequest {
    #[serde(default)]
    id: u64,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize, Default)]
struct WsResponse {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

/// Monotonic per-process counter backing the per-connection session id.
static WS_CONN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Monotonic per-process counter minting one execution id per top-level
/// request. The id rides on the `Caller` into every child runner run so the
/// trace can group a request with all the dispatches it spawned.
static EXEC_SEQ: AtomicU64 = AtomicU64::new(1);

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    // Every connection gets a stable session id (`ws-<n>`); callers can
    // override it per request via the optional `session` param when they
    // bridge an external session space (Telegram chat, Discord channel, …).
    let session = format!("ws-{}", WS_CONN_SEQ.fetch_add(1, Ordering::Relaxed));
    while let Some(msg) = socket.recv().await {
        let frame = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Binary(b)) => match std::str::from_utf8(&b) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "ws recv error");
                break;
            }
        };
        let req: WsRequest = match serde_json::from_str(&frame) {
            Ok(r) => r,
            Err(e) => {
                let resp = WsResponse {
                    id: 0,
                    ok: false,
                    code: Some("invalid_envelope".into()),
                    error: Some(e.to_string()),
                    ..Default::default()
                };
                let _ = send(&mut socket, &resp).await;
                continue;
            }
        };
        let resp = dispatch(state.clone(), req, &session).await;
        if send(&mut socket, &resp).await.is_err() {
            break;
        }
    }
}

/// Control-plane socket. Any authenticated control connection IS an approver:
/// it subscribes to the broker on connect, receives `approval.request` push
/// frames, and answers them with `approvals.resolve`. Bidirectional — unlike
/// the strictly request/response public socket.
async fn handle_control_socket(mut socket: WebSocket, state: AppState) {
    let (approver_id, mut rx) = state.broker.subscribe();
    loop {
        tokio::select! {
            // Server push: a pending approval request to relay to the operator.
            maybe = rx.recv() => {
                match maybe {
                    Some(req) => {
                        let frame = json!({ "event": "approval.request", "req": req });
                        if socket
                            .send(Message::Text(frame.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => break, // broker dropped
                }
            }
            // Client -> server: subscribe ack / resolve.
            msg = socket.recv() => {
                let frame = match msg {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Binary(b))) => match std::str::from_utf8(&b) {
                        Ok(s) => s.to_string(),
                        Err(_) => continue,
                    },
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        tracing::warn!(error = %e, "control recv error");
                        break;
                    }
                };
                let resp = control_dispatch(&state, &frame);
                if send(&mut socket, &resp).await.is_err() {
                    break;
                }
            }
        }
    }
    state.broker.unsubscribe(approver_id);
}

#[derive(Deserialize)]
struct ResolveParams {
    request_id: u64,
    verdict: agentd_types::Verdict,
}

/// Handle one control envelope. Only `approvals.*` for now.
fn control_dispatch(state: &AppState, frame: &str) -> WsResponse {
    let req: WsRequest = match serde_json::from_str(frame) {
        Ok(r) => r,
        Err(e) => return err(0, "invalid", e.to_string()),
    };
    let id = req.id;
    match req.method.as_str() {
        // Subscription happens on connect; this is an idempotent ack.
        "approvals.subscribe" => ok(id, json!({ "subscribed": true })),
        "approvals.resolve" => match serde_json::from_value::<ResolveParams>(req.params) {
            Ok(p) => {
                state.broker.resolve(p.request_id, p.verdict);
                ok(id, json!({ "resolved": p.request_id }))
            }
            Err(e) => err(id, "invalid", e.to_string()),
        },
        other => err(id, "invalid", format!("unknown control method `{other}`")),
    }
}

async fn send(socket: &mut WebSocket, resp: &WsResponse) -> Result<(), axum::Error> {
    // WsResponse is always serializable; the fallback only exists so a freak
    // encode error closes the frame cleanly instead of panicking the task.
    let body = serde_json::to_string(resp).unwrap_or_else(|e| {
        format!(
            r#"{{"id":{},"ok":false,"code":"serialize_failed","error":{}}}"#,
            resp.id,
            serde_json::to_string(&e.to_string()).unwrap_or_else(|_| "\"encode error\"".into()),
        )
    });
    socket.send(Message::Text(body.into())).await
}

/// Build the `Caller` for one request: interface is always `ws`; session
/// defaults to the connection id but a request-level `session` param wins.
/// The handshake bearer token authenticates the *connection*; `user` is still
/// caller-supplied identity within that trusted channel, not separately verified.
fn ws_caller(conn_session: &str, session: Option<String>, user: Option<String>) -> Caller {
    let exec_id = format!("exec-{}", EXEC_SEQ.fetch_add(1, Ordering::Relaxed));
    let mut c = Caller::interface("ws")
        .with_session(session.unwrap_or_else(|| conn_session.to_string()))
        .with_execution(exec_id);
    if let Some(u) = user {
        c = c.with_user(u);
    }
    c
}

async fn dispatch(state: AppState, req: WsRequest, conn_session: &str) -> WsResponse {
    let id = req.id;
    // Pin the current runtime for this request. A concurrent hot-reload swap
    // only affects requests dispatched after it; this one finishes on `executor`.
    let executor = state.executor.load();
    match req.method.as_str() {
        "health" => ok(id, json!("ok")),
        "tools.list" => ok(id, json!(executor.registry().list())),

        "actions.call" => match serde_json::from_value::<CallParams>(req.params) {
            Ok(p) => {
                let call = ActionCall {
                    action: p.name,
                    args: p.args.unwrap_or(Value::Null),
                };
                let caller = ws_caller(conn_session, p.session, p.user);
                match executor.run(caller, call).await {
                    Ok((res, dur)) => ok(id, json!({ "result": res.value, "duration_ms": dur })),
                    Err((e, dur)) => action_error(id, e, dur),
                }
            }
            Err(e) => bad_params(id, e),
        },

        "runners.list" => ok(
            id,
            json!(
                executor
                    .runners()
                    .list()
                    .into_iter()
                    .map(|d| json!({
                        "name": d.name,
                        "model": d.model,
                        "skills": d.skills,
                        "allowed_actions": d.allowed_actions,
                    }))
                    .collect::<Vec<_>>()
            ),
        ),
        "runners.inspect" => match serde_json::from_value::<NameParam>(req.params) {
            Ok(p) => {
                let Some(def) = executor.runners().get(&p.name) else {
                    return err(id, "not_found", format!("runner `{}` not found", p.name));
                };
                match compose(&def, executor.skills()) {
                    Ok(c) => ok_ser(id, &c),
                    Err(e) => err(id, "compose_failed", e.to_string()),
                }
            }
            Err(e) => bad_params(id, e),
        },
        "runners.run" => match serde_json::from_value::<RunParams>(req.params) {
            Ok(p) => {
                let caller = ws_caller(conn_session, p.session, p.user).with_runner(p.name.clone());
                match executor.run_runner(caller, &p.name, p.prompt).await {
                    Ok(out) => ok_ser(id, &out),
                    Err(e) => runner_error(id, e),
                }
            }
            Err(e) => bad_params(id, e),
        },

        "skills.list" => ok(
            id,
            json!(
                executor
                    .skills()
                    .list()
                    .into_iter()
                    .map(|d| json!({
                        "name": d.name,
                        "description": d.description,
                        "actions": d.actions,
                    }))
                    .collect::<Vec<_>>()
            ),
        ),
        "skills.inspect" => match serde_json::from_value::<NameParam>(req.params) {
            Ok(p) => match executor.skills().get(&p.name) {
                Some(def) => ok_ser(id, &def),
                None => err(id, "not_found", format!("skill `{}` not found", p.name)),
            },
            Err(e) => bad_params(id, e),
        },

        "services.list" => ok_ser(id, &executor.services().statuses()),

        other => err(id, "unknown_method", format!("unknown method `{other}`")),
    }
}

#[derive(Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    args: Option<Value>,
    /// Override the per-connection session id (e.g. a Telegram chat id).
    #[serde(default)]
    session: Option<String>,
    /// End-user id as seen by the bridging interface.
    #[serde(default)]
    user: Option<String>,
}

#[derive(Deserialize)]
struct NameParam {
    name: String,
}

#[derive(Deserialize)]
struct RunParams {
    name: String,
    prompt: String,
    /// Override the per-connection session id (e.g. a Telegram chat id).
    #[serde(default)]
    session: Option<String>,
    /// End-user id as seen by the bridging interface.
    #[serde(default)]
    user: Option<String>,
}

fn ok(id: u64, result: Value) -> WsResponse {
    WsResponse {
        id,
        ok: true,
        result: Some(result),
        ..Default::default()
    }
}

/// Like [`ok`] but for values that still need serializing. Domain types here
/// all derive `Serialize` so this never fails in practice — but a request
/// handler is the wrong place to panic, so a serialization error becomes a
/// normal error envelope instead.
fn ok_ser<T: Serialize>(id: u64, value: &T) -> WsResponse {
    match serde_json::to_value(value) {
        Ok(v) => ok(id, v),
        Err(e) => err(id, "serialize_failed", e.to_string()),
    }
}

fn err(id: u64, code: impl Into<String>, msg: impl Into<String>) -> WsResponse {
    WsResponse {
        id,
        ok: false,
        code: Some(code.into()),
        error: Some(msg.into()),
        ..Default::default()
    }
}

fn bad_params(id: u64, e: serde_json::Error) -> WsResponse {
    err(id, "bad_params", e.to_string())
}

fn action_error(id: u64, e: RegistryError, dur: u128) -> WsResponse {
    let code = match &e {
        RegistryError::NotFound(_) => "not_found",
        RegistryError::Denied { .. } => "denied",
        RegistryError::NeedsConfirmation(_) => "needs_confirmation",
        RegistryError::Invocation(_) => "invocation_failed",
    };
    WsResponse {
        id,
        ok: false,
        code: Some(code.into()),
        error: Some(e.to_string()),
        result: Some(json!({ "duration_ms": dur })),
    }
}

fn runner_error(id: u64, e: RunnerError) -> WsResponse {
    let code = match &e {
        RunnerError::NotFound(_) => "not_found",
        RunnerError::UnknownSkill { .. } => "unknown_skill",
        RunnerError::NoProvider { .. } => "no_provider",
        RunnerError::Provider { .. } => "provider_upstream",
    };
    err(id, code, e.to_string())
}

impl IntoResponse for WsResponse {
    fn into_response(self) -> Response {
        axum::Json(self).into_response()
    }
}

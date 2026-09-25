//! Daemon HTTP and WebSocket interface. The authenticated data and control
//! planes use WebSocket JSON envelopes; configured webhooks use signed HTTP
//! POST requests and dispatch into the same permission-aware executor.
//!
//! Envelope shape:
//!
//! ```json
//! // client -> server
//! { "id": 1, "method": "actions.call", "params": { "name": "git.diff", "args": {} } }
//! // server -> client (success)
//! { "id": 1, "ok": true, "result": { ... } }
//! // server -> client (error; `tip` and `trace` are optional)
//! { "id": 1, "ok": false, "code": "not_found", "error": "action `x` not registered",
//!   "tip": "Run `agentctl tools` to list registered actions" }
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
//! | `runners.run`     | `{ name, prompt, session_id?, session?, user? }` | `RunnerOutcome`               |
//! | `sessions.create` | `{ label?, user?, runner? }`               | `SessionMeta`                       |
//! | `sessions.get`    | `{ id }` or `{ label }`                    | `{ ...SessionMeta, turns }`         |
//! | `sessions.list`   | `{ limit? }`                               | `[SessionMeta]`                     |
//! | `sessions.delete` | `{ id }`                                   | `{ deleted }`                       |
//! | `skills.list`     | none                                       | `[{name, description, actions}]`    |
//! | `skills.inspect`  | `{ name }`                                 | `SkillDef`                          |
//! | `services.list`   | none                                       | `[ServiceStatus]`                   |
//!
//! Caller identity: every connection gets a session id `ws-<n>`; `actions.call`
//! and `runners.run` accept optional `session` / `user` params so bridging
//! interfaces (Telegram, Discord, …) can carry their own identity space.
//! Lua handlers read it back via `ctx.caller`.

use agentd_executor::{Executor, scope_of};
use agentd_permissions::Caller;
use agentd_runners::{RunOptions, RunnerError, compose};
use agentd_types::{ActionCall, RegistryError};
pub use axum::serve;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use futures_util::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// One named HMAC-SHA256 webhook route. The secret is resolved from the OS
/// keyring by the daemon; it never appears in config.toml or the action args.
#[derive(Clone)]
pub struct Webhook {
    action: String,
    secret: Arc<String>,
    signature_header: axum::http::HeaderName,
    signature_prefix: String,
    id_header: Option<axum::http::HeaderName>,
}

impl Webhook {
    pub fn new(
        action: impl Into<String>,
        secret: impl Into<String>,
        signature_header: &str,
        signature_prefix: impl Into<String>,
        id_header: Option<&str>,
    ) -> Result<Self, String> {
        let action = action.into();
        if action.trim().is_empty() {
            return Err("webhook action is empty".into());
        }
        let secret = secret.into();
        if secret.is_empty() {
            return Err("webhook secret is empty".into());
        }
        let signature_header = signature_header
            .parse()
            .map_err(|_| format!("invalid signature header `{signature_header}`"))?;
        let id_header: Option<axum::http::HeaderName> = id_header
            .map(|name| {
                name.parse()
                    .map_err(|_| format!("invalid id header `{name}`"))
            })
            .transpose()?;
        if id_header.as_ref().is_some_and(|header| {
            header == signature_header
                || header == axum::http::header::AUTHORIZATION
                || header == axum::http::header::COOKIE
                || header.as_str() == "proxy-authorization"
        }) {
            return Err("id header cannot contain authentication data".into());
        }
        Ok(Self {
            action,
            secret: Arc::new(secret),
            signature_header,
            signature_prefix: signature_prefix.into(),
            id_header,
        })
    }
}

/// Coordinates admission and bounded draining for HTTP requests and upgraded sockets.
#[derive(Default)]
pub struct Lifecycle {
    draining: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
}

struct ActiveRequest(Arc<Lifecycle>);

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
        self.0.changed.notify_waiters();
    }
}

impl Lifecycle {
    fn enter(self: &Arc<Self>) -> ActiveRequest {
        self.active.fetch_add(1, Ordering::SeqCst);
        ActiveRequest(self.clone())
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    async fn stopping(&self) {
        loop {
            let changed = self.changed.notified();
            if self.is_draining() {
                return;
            }
            changed.await;
        }
    }

    /// Reject new work, then give existing requests time to finish.
    pub async fn drain(&self, timeout: std::time::Duration) {
        self.draining.store(true, Ordering::SeqCst);
        self.changed.notify_waiters();
        let _ = tokio::time::timeout(timeout, async {
            loop {
                let changed = self.changed.notified();
                if self.active.load(Ordering::SeqCst) == 0 {
                    break;
                }
                changed.await;
            }
        })
        .await;
    }
}

#[derive(Clone)]
pub struct AppState {
    pub lifecycle: Arc<Lifecycle>,
    /// Hot-swappable executor. `agentd --watch` rebuilds the Lua runtime and
    /// `store()`s a fresh executor here; in-flight requests keep the `Arc` they
    /// `load()`ed and drain on the old runtime. Without `--watch` the pointer
    /// never changes.
    pub executor: Arc<arc_swap::ArcSwap<Executor>>,
    /// Bearer token required on the public `/ws` handshake. `None` disables auth
    /// (the daemon's `--no-auth`); `/health` is always open for liveness probes.
    /// A connection authenticated with it is interface `ws`.
    pub auth_token: Option<Arc<String>>,
    /// Per-interface bearer tokens (`[interfaces.<name>] token = ...`), keyed
    /// by token. A connection presenting one becomes that interface, which is
    /// what `[interface.<name>]` grants and session ownership key on.
    pub interface_tokens: Arc<HashMap<String, String>>,
    /// Bearer token required on the `/control` handshake. Distinct from
    /// `auth_token` so a public-token holder can never reach the control plane.
    /// `None` disables the control gate (`--no-auth`).
    pub admin_token: Option<Arc<String>>,
    /// Approval broker, shared with the executor. The control plane registers
    /// operator clients against it (`subscribe`) and relays their verdicts
    /// (`resolve`).
    pub broker: Arc<agentd_approvals::Broker>,
    /// Named routes served at `/webhooks/<name>`.
    pub webhooks: Arc<HashMap<String, Webhook>>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(health))
        .route("/ws", any(ws_upgrade))
        .route("/control", any(control_upgrade))
        .route("/webhooks/{name}", post(webhook))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            admission,
        ))
        .with_state(state)
}

async fn admission(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let _active = state.lifecycle.enter();
    if state.lifecycle.is_draining() && request.uri().path() != "/health" {
        return (StatusCode::SERVICE_UNAVAILABLE, "daemon is draining").into_response();
    }
    next.run(request).await
}

async fn health() -> &'static str {
    "ok"
}

async fn webhook(
    Path(name): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(webhook) = state.webhooks.get(&name) else {
        return webhook_error(StatusCode::NOT_FOUND, "webhook not found");
    };

    let signature = headers
        .get(&webhook.signature_header)
        .and_then(|value| value.to_str().ok());
    if !valid_webhook_signature(webhook, signature, &body) {
        return webhook_error(StatusCode::UNAUTHORIZED, "invalid signature");
    }

    let request_id = match &webhook.id_header {
        Some(header) => match header_value(&headers, header) {
            Some(value) => value,
            None => return webhook_error(StatusCode::BAD_REQUEST, "missing webhook ID header"),
        },
        None => format!(
            "webhook-{name}-{}",
            WEBHOOK_REQ_SEQ.fetch_add(1, Ordering::Relaxed)
        ),
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return webhook_error(StatusCode::BAD_REQUEST, "body must be valid JSON"),
    };

    let args = json!({
        "headers": forwarded_headers(&headers, &webhook.signature_header),
        "payload": payload,
    });
    let caller = webhook_caller(&name, &request_id);
    let executor = state.executor.load();
    let call = ActionCall {
        action: webhook.action.clone(),
        args,
    };
    match executor.run(caller, call).await {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(json!({ "ok": true, "request_id": request_id })),
        )
            .into_response(),
        Err((error, _)) => {
            tracing::error!(
                webhook = %name,
                action = %webhook.action,
                request_id = %request_id,
                error = %error,
                "webhook action failed"
            );
            webhook_error(StatusCode::INTERNAL_SERVER_ERROR, "webhook action failed")
        }
    }
}

fn header_value(headers: &HeaderMap, name: &axum::http::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn forwarded_headers(
    headers: &HeaderMap,
    signature_header: &axum::http::HeaderName,
) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| {
            *name != signature_header
                && *name != axum::http::header::AUTHORIZATION
                && *name != axum::http::header::COOKIE
                && name.as_str() != "proxy-authorization"
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

fn valid_webhook_signature(webhook: &Webhook, signature: Option<&str>, body: &[u8]) -> bool {
    let Some(encoded) = signature.and_then(|value| value.strip_prefix(&webhook.signature_prefix))
    else {
        return false;
    };
    let Ok(expected) = hex::decode(encoded) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(webhook.secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

fn webhook_caller(name: &str, request_id: &str) -> Caller {
    Caller::interface(format!("webhook.{name}"))
        .with_session(request_id.to_string())
        .with_execution(next_execution_id())
}

fn webhook_error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(json!({ "ok": false, "error": message }))).into_response()
}

/// Constant-time equality so token checks do not leak prefix matches.
fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Which interface a `/ws` handshake is. The public token (or `--no-auth`)
/// means `ws`; an interface token names its interface; anything else is refused.
fn resolve_interface(state: &AppState, headers: &axum::http::HeaderMap) -> Option<String> {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if let Some(p) = presented {
        // Scan every entry so timing does not reveal which token matched.
        let mut hit: Option<&String> = None;
        for (token, name) in state.interface_tokens.iter() {
            if token_eq(token, p) {
                hit = Some(name);
            }
        }
        if let Some(name) = hit {
            return Some(name.clone());
        }
    }
    match &state.auth_token {
        None => Some("ws".to_string()),
        Some(token) => presented
            .filter(|p| token_eq(p, token))
            .map(|_| "ws".to_string()),
    }
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(interface) = resolve_interface(&state, &headers) else {
        return (axum::http::StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    };
    ws.max_message_size(1_100_000)
        .max_frame_size(1_100_000)
        .on_upgrade(move |socket| handle_socket(socket, state, interface))
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
    ws.max_message_size(1_100_000)
        .max_frame_size(1_100_000)
        .on_upgrade(move |socket| handle_control_socket(socket, state))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_ms: Option<u64>,
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    /// Actionable hint tied to `code` (e.g. where to configure providers).
    #[serde(skip_serializing_if = "Option::is_none")]
    tip: Option<String>,
    /// Cleaned script traceback frames, innermost first.
    #[serde(skip_serializing_if = "Option::is_none")]
    trace: Option<Vec<String>>,
}

/// Monotonic per-process counter backing the per-connection session id.
static WS_CONN_SEQ: AtomicU64 = AtomicU64::new(1);

/// Monotonic per-process counter minting one execution id per top-level
/// request. The id rides on the `Caller` into every child runner run so the
/// trace can group a request with all the dispatches it spawned.
static EXEC_SEQ: AtomicU64 = AtomicU64::new(1);

/// Monotonic fallback request id for webhook routes without `id_header`.
static WEBHOOK_REQ_SEQ: AtomicU64 = AtomicU64::new(1);

const MAX_IN_FLIGHT: usize = 32;
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

struct Outbound {
    frame: Value,
    finished: Option<u64>,
}

async fn handle_socket(mut socket: WebSocket, state: AppState, interface: String) {
    let _connection = state.lifecycle.enter();
    let mut draining = state.lifecycle.is_draining();
    let session = format!("ws-{}", WS_CONN_SEQ.fetch_add(1, Ordering::Relaxed));
    let conn = Arc::new(ConnIdentity { interface, session });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Outbound>(64);
    let mut tasks = FuturesUnordered::<BoxFuture<'static, ()>>::new();
    let mut active = HashMap::<u64, Option<tokio::sync::oneshot::Sender<()>>>::new();
    loop {
        if draining && active.is_empty() {
            break;
        }
        tokio::select! {
            _ = state.lifecycle.stopping(), if !draining => { draining = true; }
            Some(out) = rx.recv() => {
                if let Some(id) = out.finished {
                    active.remove(&id);
                }
                if !matches!(tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Text(out.frame.to_string().into()))).await, Ok(Ok(()))) {
                    break;
                }
            }
            Some(()) = tasks.next(), if !tasks.is_empty() => {}
            msg = socket.recv(), if !draining => {
                let frame = match msg {
                    Some(Ok(Message::Text(t))) => t.to_string(),
                    Some(Ok(Message::Binary(b))) => match String::from_utf8(b.to_vec()) {
                        Ok(s) => s,
                        Err(_) => break,
                    },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => continue,
                };
                let req: WsRequest = match serde_json::from_str(&frame) {
                    Ok(req) => req,
                    Err(e) => {
                        if send(&mut socket, &err(0, "invalid_envelope", e.to_string())).await.is_err() { break; }
                        continue;
                    }
                };
                if active.contains_key(&req.id) {
                    // Reusing an active id makes response correlation ambiguous.
                    break;
                }
                if req.method == "runners.cancel" {
                    #[derive(Deserialize)]
                    struct CancelParams { id: u64 }
                    let response = match serde_json::from_value::<CancelParams>(req.params) {
                        Ok(p) => match active.get_mut(&p.id).and_then(Option::take) {
                            Some(cancel) => {
                                let accepted = cancel.send(()).is_ok();
                                ok(req.id, json!({ "cancelled": accepted }))
                            }
                            None => ok(req.id, json!({ "cancelled": false })),
                        },
                        Err(e) => bad_params(req.id, e),
                    };
                    if send(&mut socket, &response).await.is_err() { break; }
                    continue;
                }
                if active.len() >= MAX_IN_FLIGHT {
                    if send(&mut socket, &err(req.id, "busy", "connection has 32 in-flight requests")).await.is_err() { break; }
                    continue;
                }
                let state = state.clone();
                let conn = conn.clone();
                let tx = tx.clone();
                let id = req.id;
                let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
                let is_runner = req.method == "runners.run";
                active.insert(id, is_runner.then_some(cancel_tx));
                tasks.push(Box::pin(async move {
                    let response = if is_runner {
                        match serde_json::from_value::<RunParams>(req.params) {
                            Ok(p) => {
                                tokio::select! {
                                    result = handle_run(&state, id, p, &conn, &tx) => result,
                                    _ = cancel_rx => err(id, "cancelled", "runner cancelled; completed side effects are not undone"),
                                }
                            }
                            Err(e) => bad_params(id, e),
                        }
                    } else {
                        dispatch(state, req, &conn).await
                    };
                    let _ = tx.send(Outbound { frame: json!(response), finished: Some(id) }).await;
                }));
            }
        }
    }
    // Dropping the in-flight futures stops further model turns on disconnect.
    // Already dispatched external side effects cannot be rolled back.
}

async fn handle_run(
    state: &AppState,
    id: u64,
    p: RunParams,
    conn: &ConnIdentity,
    outbound: &tokio::sync::mpsc::Sender<Outbound>,
) -> WsResponse {
    let executor = state.executor.load_full();
    let caller = ws_caller(conn, p.session, p.user).with_runner(p.name.clone());
    let (tx, mut rx) = agentd_ai::types::stream_channel();
    let run = executor.run_runner_with_options(caller, &p.name, p.options, p.stream.then_some(tx));
    tokio::pin!(run);
    loop {
        tokio::select! {
            Some(ev) = rx.recv(), if p.stream => {
                if outbound.send(Outbound { frame: json!({ "event": "runner.delta", "id": id, "delta": ev }), finished: None }).await.is_err() {
                    return err(id, "slow_consumer", "stream consumer could not keep up; run stopped");
                }
            }
            res = &mut run => {
                while let Ok(ev) = rx.try_recv() {
                    if outbound.send(Outbound { frame: json!({ "event": "runner.delta", "id": id, "delta": ev }), finished: None }).await.is_err() {
                        return err(id, "slow_consumer", "stream consumer could not keep up");
                    }
                }
                return res.map_or_else(|e| runner_error(id, e), |out| ok_ser(id, &out));
            }
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
    tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Text(body.into())))
        .await
        .map_err(axum::Error::new)?
}

/// What the handshake established for one connection.
struct ConnIdentity {
    /// `ws` for the public token, otherwise the `[interfaces.<name>]` that matched.
    interface: String,
    /// Auto-minted `ws-<n>` connection id.
    session: String,
}

/// Build the `Caller` for one request: the interface comes from the handshake
/// token; session defaults to the connection id but a request-level `session`
/// param wins. `user` is identity the interface vouches for within its own
/// channel; the daemon trusts the interface, not the end user.
fn ws_caller(conn: &ConnIdentity, session: Option<String>, user: Option<String>) -> Caller {
    let mut c = Caller::interface(conn.interface.as_str())
        .with_session(session.unwrap_or_else(|| conn.session.clone()))
        .with_execution(next_execution_id());
    if let Some(u) = user {
        c = c.with_user(u);
    }
    c
}

fn next_execution_id() -> String {
    format!("exec-{}", EXEC_SEQ.fetch_add(1, Ordering::Relaxed))
}

async fn dispatch(state: AppState, req: WsRequest, conn: &ConnIdentity) -> WsResponse {
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
                let caller = ws_caller(conn, p.session, p.user);
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
                let caller = ws_caller(conn, p.session, p.user).with_runner(p.name.clone());
                match executor
                    .run_runner_with_options(caller, &p.name, p.options, None)
                    .await
                {
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

        "sessions.create" => {
            let Some(store) = executor.sessions() else {
                return no_sessions(id);
            };
            match serde_json::from_value::<CreateSessionParams>(req.params) {
                Ok(p) => {
                    let caller = ws_caller(conn, p.session, p.user);
                    let new = agentd_sessions::NewSession::in_scope(&scope_of(&caller))
                        .label(p.label)
                        .runner(p.runner);
                    match store.create(new) {
                        Ok(meta) => ok_ser(id, &meta),
                        Err(e) => session_error(id, e),
                    }
                }
                Err(e) => bad_params(id, e),
            }
        }
        "sessions.get" => {
            let Some(store) = executor.sessions() else {
                return no_sessions(id);
            };
            let p = match serde_json::from_value::<SessionRef>(req.params) {
                Ok(p) => p,
                Err(e) => return bad_params(id, e),
            };
            let scope = scope_of(&ws_caller(conn, p.session.clone(), p.user.clone()));
            let meta = match (&p.id, &p.label) {
                (Some(i), _) => store.get_in(&scope, i),
                (None, Some(l)) => store.find_in(&scope, l),
                (None, None) => {
                    return err(id, "bad_params", "pass a session `id` or `label`");
                }
            };
            match meta {
                Ok(Some(meta)) => match store.turns(&meta.id) {
                    Ok(turns) => {
                        let mut v = serde_json::to_value(&meta).unwrap_or_default();
                        v["turns"] = serde_json::to_value(turns).unwrap_or_default();
                        ok(id, v)
                    }
                    Err(e) => session_error(id, e),
                },
                Ok(None) => err(
                    id,
                    "session_not_found",
                    format!(
                        "session `{}` does not exist",
                        p.id.or(p.label).unwrap_or_default()
                    ),
                ),
                Err(e) => session_error(id, e),
            }
        }
        "sessions.list" => {
            let Some(store) = executor.sessions() else {
                return no_sessions(id);
            };
            let params = if req.params.is_null() {
                json!({})
            } else {
                req.params
            };
            match serde_json::from_value::<LimitParam>(params) {
                Ok(p) => match store.list(
                    &scope_of(&ws_caller(conn, p.session, p.user)),
                    p.limit.unwrap_or(50).min(500),
                ) {
                    Ok(v) => ok_ser(id, &v),
                    Err(e) => session_error(id, e),
                },
                Err(e) => bad_params(id, e),
            }
        }
        "sessions.rename" => {
            let Some(store) = executor.sessions() else {
                return no_sessions(id);
            };
            match serde_json::from_value::<RenameParams>(req.params) {
                Ok(p) => {
                    let scope = scope_of(&ws_caller(conn, p.session, p.user));
                    match store.get_in(&scope, &p.id) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            return err(
                                id,
                                "session_not_found",
                                format!("session `{}` does not exist", p.id),
                            );
                        }
                        Err(e) => return session_error(id, e),
                    }
                    match store.relabel(&p.id, p.label) {
                        Ok(meta) => ok_ser(id, &meta),
                        Err(e) => session_error(id, e),
                    }
                }
                Err(e) => bad_params(id, e),
            }
        }
        "sessions.delete" => {
            let Some(store) = executor.sessions() else {
                return no_sessions(id);
            };
            match serde_json::from_value::<IdParam>(req.params) {
                Ok(p) => {
                    let scope = scope_of(&ws_caller(conn, p.session, p.user));
                    // Out-of-scope ids delete nothing and say so like a missing id would.
                    let visible = match store.get_in(&scope, &p.id) {
                        Ok(v) => v.is_some(),
                        Err(e) => return session_error(id, e),
                    };
                    if !visible {
                        return ok(id, json!({ "deleted": false }));
                    }
                    match store.delete(&p.id) {
                        Ok(deleted) => ok(id, json!({ "deleted": deleted })),
                        Err(e) => session_error(id, e),
                    }
                }
                Err(e) => bad_params(id, e),
            }
        }

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

/// `sessions.create`. The owner is the connection's interface; `user` is the
/// same caller-identity override every other method takes.
#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateSessionParams {
    label: Option<String>,
    runner: Option<String>,
    session: Option<String>,
    user: Option<String>,
}

#[derive(Deserialize)]
struct IdParam {
    id: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

/// `sessions.rename`: a missing or null `label` clears it.
#[derive(Deserialize)]
struct RenameParams {
    id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LimitParam {
    limit: Option<usize>,
    session: Option<String>,
    user: Option<String>,
}

/// `sessions.get` accepts either the daemon id or the caller's label.
#[derive(Deserialize, Default)]
#[serde(default)]
struct SessionRef {
    id: Option<String>,
    label: Option<String>,
    session: Option<String>,
    user: Option<String>,
}

fn no_sessions(id: u64) -> WsResponse {
    err(
        id,
        "sessions_unavailable",
        "no session store is configured on this daemon",
    )
}

fn session_error(id: u64, e: agentd_sessions::SessionError) -> WsResponse {
    let code = match &e {
        agentd_sessions::SessionError::NotFound(_) => "session_not_found",
        agentd_sessions::SessionError::LabelTaken(_) => "session_label_taken",
        agentd_sessions::SessionError::Backend(_) => "session_store",
    };
    err(id, code, e.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunParams {
    name: String,
    #[serde(flatten)]
    options: RunOptions,
    /// Override the per-connection session id (e.g. a Telegram chat id).
    #[serde(default)]
    session: Option<String>,
    /// End-user id as seen by the bridging interface.
    #[serde(default)]
    user: Option<String>,
    /// Push `runner.delta` event frames while the run is in flight. The
    /// final complete response envelope is sent either way.
    #[serde(default)]
    stream: bool,
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

/// Actionable hint for an error code, shown to humans next to the message.
fn tip_for(code: &str) -> Option<String> {
    let tip = match code {
        "no_provider" => {
            "You can configure new providers in your `config.toml` — https://docs.podo.fun/agentd/v0/reference/configuration"
        }
        "not_found" => "Run `agentctl tools` to list registered actions",
        "runner_not_found" => "Run `agentctl runner ls` to list runners",
        "session_not_found" => {
            "Run `agentctl session ls` to list sessions, or `agentctl session new` to start one"
        }
        "session_busy" => "Wait for the in-flight run on this session to finish, then retry",
        "session_label_taken" => "Fetch the existing one with `sessions.get { label }` instead",
        "denied" | "needs_confirmation" => {
            "Grants live in `grants.toml`; run `agentctl grants listen` to approve interactively"
        }
        "unknown_skill" => "Run `agentctl skill ls` to list skills",
        "provider_misconfigured" => {
            "Store the API key with `agentctl secret set <name> <value>` — https://docs.podo.fun/agentd/v0/providers/credentials"
        }
        "bad_params" => "Pass args as `-d key=value` or `-j '<json>'`",
        _ => return None,
    };
    Some(tip.to_string())
}

fn err(id: u64, code: impl Into<String>, msg: impl Into<String>) -> WsResponse {
    let code = code.into();
    WsResponse {
        id,
        ok: false,
        tip: tip_for(&code),
        code: Some(code),
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
        RegistryError::Script { .. } => "lua_error",
    };
    let trace = match &e {
        RegistryError::Script { trace, .. } if !trace.is_empty() => Some(trace.clone()),
        _ => None,
    };
    WsResponse {
        id,
        ok: false,
        tip: tip_for(code),
        trace,
        code: Some(code.into()),
        error: Some(e.to_string()),
        result: Some(json!({ "duration_ms": dur })),
        ..Default::default()
    }
}

fn runner_error(id: u64, e: RunnerError) -> WsResponse {
    let code = match &e {
        RunnerError::Denied(_) => "denied",
        RunnerError::InvalidInput(_) => "bad_params",
        RunnerError::Timeout => "timeout",
        RunnerError::Busy => "busy",
        RunnerError::SlowConsumer => "slow_consumer",

        RunnerError::NotFound(_) => "runner_not_found",
        RunnerError::SessionNotFound(_) => "session_not_found",
        RunnerError::SessionBusy(_) => "session_busy",
        RunnerError::Session(_) => "session_store",
        RunnerError::UnknownSkill { .. } => "unknown_skill",
        RunnerError::NoProvider { .. } => "no_provider",
        RunnerError::Provider {
            source: agentd_ai::ProviderError::Config(_),
            ..
        } => "provider_misconfigured",
        RunnerError::Provider { .. } => "provider_upstream",
    };
    let mut response = err(id, code, e.to_string());
    if let RunnerError::Provider {
        source:
            agentd_ai::ProviderError::Http {
                status,
                retry_after_ms,
                ..
            },
        ..
    } = e
    {
        response.provider_status = Some(status);
        response.retry_after_ms = retry_after_ms;
    }
    response
}

impl IntoResponse for WsResponse {
    fn into_response(self) -> Response {
        axum::Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_signature_matches_standard_hmac_sha256_vector() {
        let webhook =
            Webhook::new("events.ingest", "\x0b".repeat(20), "x-signature", "", None).unwrap();
        assert!(valid_webhook_signature(
            &webhook,
            Some("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"),
            b"Hi There"
        ));
    }

    #[test]
    fn webhook_rejects_invalid_header_configuration() {
        assert!(Webhook::new("x", "secret", "not a header", "", None).is_err());
        assert!(Webhook::new("x", "secret", "x-signature", "", Some("bad header")).is_err());
        assert!(Webhook::new("x", "secret", "x-signature", "", Some("x-signature")).is_err());
        assert!(Webhook::new("x", "secret", "x-signature", "", Some("authorization")).is_err());
        assert!(Webhook::new("", "secret", "x-signature", "", None).is_err());
        assert!(Webhook::new("x", "", "x-signature", "", None).is_err());
    }

    #[test]
    fn script_error_serializes_tip_and_trace() {
        let resp = action_error(
            7,
            RegistryError::Script {
                message: "boom".into(),
                trace: vec!["init.lua:53".into()],
            },
            12,
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["code"], "lua_error");
        assert_eq!(v["error"], "boom");
        assert_eq!(v["trace"][0], "init.lua:53");
        assert!(v.get("tip").is_none(), "lua_error has no tip");
    }

    #[test]
    fn misconfigured_provider_gets_own_code_and_tip() {
        let resp = runner_error(
            3,
            RunnerError::Provider {
                provider: "github".into(),
                source: agentd_ai::ProviderError::Config(
                    "provider `github` is not configured — store the API key in the \
                     `github_models_token` secret to use it"
                        .into(),
                ),
            },
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["code"], "provider_misconfigured");
        let msg = v["error"].as_str().unwrap();
        // Config messages pass through bare — one sentence, provider named once.
        assert_eq!(msg.matches("`github`").count(), 1, "{msg}");
        assert_eq!(msg.matches(':').count(), 0, "no colon chains, got: {msg}");
        assert!(v["tip"].as_str().unwrap().contains("docs.podo.fun"));
    }

    #[test]
    fn empty_trace_and_absent_tip_are_omitted() {
        let resp = action_error(
            1,
            RegistryError::Script {
                message: "x".into(),
                trace: vec![],
            },
            0,
        );
        let v = serde_json::to_value(&resp).unwrap();
        assert!(v.get("trace").is_none());

        let resp = err(2, "no_provider", "m");
        let v = serde_json::to_value(&resp).unwrap();
        assert!(
            v["tip"]
                .as_str()
                .unwrap()
                .starts_with("You can configure new providers in your `config.toml`")
        );
    }
}

//! Cooperative scheduler that drives Lua coroutines, performing blocking IO
//! outside the Lua mutex so multiple coroutines can be in flight at once.
//!
//! Protocol:
//!
//! 1. Every Lua execution (action handler body, service body, `async(fn)`
//!    callback) is wrapped in an `mlua::Thread` (coroutine).
//! 2. The scheduler calls `thread.resume(args)` inside `spawn_blocking` —
//!    holding the single Lua `Mutex<Lua>` only for that brief step.
//! 3. If the coroutine yielded an `Op` userdata, the scheduler decodes the
//!    `Op`, performs the underlying IO asynchronously (no Lua mutex held),
//!    then resumes the coroutine with the result.
//! 4. If the coroutine returned, we're done and the scheduler hands back the
//!    final value.
//!
//! Yieldable bindings (`http.get`, `ws:recv`, `ai.ask`, `shell.exec`,
//! `await(h)`, `agentd.sleep`) construct an `Op`, wrap it in `OpMarker`
//! userdata, and call `coroutine.yield(marker)`. When invoked outside any
//! coroutine (e.g. from `init.lua` top level), they fall back to
//! `tokio::runtime::Handle::block_on` for the same effect — at the cost of
//! blocking the calling thread.

use agentd_ai::{CompletionRequest, CompletionResponse, Provider};
use agentd_net::http::{Request as HttpRequest, Response as HttpResponse, send as http_send};
use agentd_net::mailer::{Mail, Mailer, SendOutcome};
use agentd_net::ws::{Connection as WsConnection, Frame as WsFrame};
use agentd_permissions::Caller;
use agentd_shell::{ExecRequest, ExecResult, exec as shell_exec};
use agentd_types::RunnerDispatcher;
use mlua::{Function, Lua, MultiValue, Table, Thread, ThreadStatus, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

/// Restores `ActiveContext::default()` on Lua app-data when dropped, so a
/// step that yields w/o another coroutine entering still cleans up.
struct AppDataGuard<'a> {
    lua: &'a Lua,
}

impl<'a> AppDataGuard<'a> {
    fn new(lua: &'a Lua) -> Self {
        Self { lua }
    }
}

impl<'a> Drop for AppDataGuard<'a> {
    fn drop(&mut self) {
        self.lua.set_app_data(crate::ActiveContext::default());
    }
}

/// What a yielding binding asks the scheduler to do. Stays in Rust land — the
/// only thing Lua sees is an opaque [`OpMarker`] userdata.
///
/// Add a new variant when (and only when) a new yieldable binding ships.
/// Future candidates: `Sleep(Duration)` for `agentd.sleep(ms)`, `WsRecv` once
/// the ws handle moves from userdata to a table so its methods can be Lua-
/// wrapped like the http/ai bindings.
pub(crate) enum Op {
    Http(HttpRequest),
    Shell(ExecRequest),
    Ai {
        provider_name: String,
        provider: Arc<dyn Provider>,
        request: CompletionRequest,
    },
    Sleep(std::time::Duration),
    Await(Arc<AsyncHandleState>),
    ChannelRecv(Arc<crate::channels::ChannelState>),
    WsSendText {
        conn: Arc<WsConnection>,
        msg: String,
    },
    WsSendBinary {
        conn: Arc<WsConnection>,
        bytes: Vec<u8>,
    },
    WsRecv {
        conn: Arc<WsConnection>,
        timeout: Option<std::time::Duration>,
    },
    WsClose {
        conn: Arc<WsConnection>,
    },
    RunnerRun {
        dispatcher: Arc<dyn RunnerDispatcher>,
        caller: Caller,
        name: String,
        opts: serde_json::Value,
    },
    MailerSend {
        mailer: Arc<Mailer>,
        mail: Mail,
    },
}

/// Userdata wrapper Lua passes back through `coroutine.yield`. The scheduler
/// pulls the `Op` out via `take` (consuming it) on the way out of Lua.
pub(crate) struct OpMarker(Mutex<Option<Op>>);

impl OpMarker {
    pub(crate) fn new(op: Op) -> Self {
        Self(Mutex::new(Some(op)))
    }
    fn take(&self) -> Option<Op> {
        self.0.lock().unwrap().take()
    }
}

impl mlua::UserData for OpMarker {}

/// Shared state for an `async(fn)` handle. The driver future writes either
/// `Ok(result)` or `Err(message)` then notifies waiters.
pub(crate) struct AsyncHandleState {
    pub slot: Mutex<Option<Result<serde_json::Value, String>>>,
    pub notify: Notify,
}

impl AsyncHandleState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(None),
            notify: Notify::new(),
        })
    }
    pub fn set(&self, value: Result<serde_json::Value, String>) {
        *self.slot.lock().unwrap() = Some(value);
        self.notify.notify_waiters();
    }
    pub fn read(&self) -> Option<Result<serde_json::Value, String>> {
        self.slot.lock().unwrap().clone()
    }
}

/// Lua-side handle for `async(fn) -> h`. Wraps the shared state so multiple
/// `await(h)` calls land on the same notifier.
#[derive(Clone)]
pub struct AsyncHandle(pub Arc<AsyncHandleState>);

impl mlua::UserData for AsyncHandle {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("is_done", |_, this, ()| {
            Ok(this.0.slot.lock().unwrap().is_some())
        });
        methods.add_method("status", |_, this, ()| {
            Ok(if this.0.slot.lock().unwrap().is_some() {
                "done"
            } else {
                "pending"
            })
        });
    }
}

/// Message queued by `async(fn)` for the background runtime to drive.
pub struct AsyncTask {
    pub thread: Thread,
    pub state: Arc<AsyncHandleState>,
    /// ActiveContext inherited from the coroutine that called `async(fn)`.
    /// Without this, the spawned task would resume with a default (empty)
    /// context and every `ctx.*` call inside the async body
    /// would deny on permissions.
    pub ctx: crate::ActiveContext,
}

/// Sender stashed in Lua app-data. The daemon spawns the matching receiver
/// task that consumes `AsyncTask`s and drives each through `scheduler::drive`
/// on its own Tokio task — that's where parallel `async(fn)` callbacks come
/// from. Cloneable (mpsc::UnboundedSender is just an Arc internally), and it
/// holds no reference to the Lua state, so storing it as app-data does not
/// create a reference cycle.
#[derive(Clone)]
pub struct AsyncTaskSpawner(pub tokio::sync::mpsc::UnboundedSender<AsyncTask>);

/// Outcome of `drive(...)`. Either the coroutine returned a value (as JSON,
/// since we cross the Lua/Tokio boundary repeatedly), or it errored.
#[derive(Debug)]
pub enum DriveError {
    Lua(mlua::Error),
    Join(tokio::task::JoinError),
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriveError::Lua(e) => write!(f, "{e}"),
            DriveError::Join(e) => write!(f, "internal join error: {e}"),
        }
    }
}
impl std::error::Error for DriveError {}

/// Drive a Lua coroutine to completion, handling yield/resume around any
/// `OpMarker` yields. Returns whatever the coroutine's body produced,
/// converted to JSON (`null` if no return value).
pub async fn drive(
    lua: Arc<Mutex<Lua>>,
    thread: Thread,
    initial_args: Vec<serde_json::Value>,
    mut ctx: crate::ActiveContext,
) -> Result<serde_json::Value, DriveError> {
    let mut next: Vec<serde_json::Value> = initial_args;
    loop {
        // Move `next` into the blocking step; we get fresh args from the op
        // result on the way back. `lua` and `thread` are reference handles
        // (Arc / mlua RegistryKey-backed) — cloning them bumps a refcount,
        // it does NOT duplicate the Lua state.
        let lua_handle = lua.clone();
        let thread_handle = thread.clone();
        let args_in = std::mem::take(&mut next);
        let ctx_step = ctx.clone();

        let step = tokio::task::spawn_blocking(
            move || -> Result<(StepOutcome, crate::ActiveContext), mlua::Error> {
                let lua = lua_handle.lock().unwrap();
                // Bind THIS coroutine's ActiveContext for the duration of this
                // resume. Restored to default at the end of the step so a
                // concurrent runner / service that resumes between our yields
                // can swap in its own context without seeing leftover state.
                lua.set_app_data(ctx_step);
                let _restore = AppDataGuard::new(&lua);
                let args = json_to_multivalue(&lua, args_in)?;
                let yielded: MultiValue = thread_handle.resume(args)?;
                let status = thread_handle.status();
                // Snapshot the (possibly mutated) context BEFORE the guard
                // resets it, so cwd changes made this step — `ctx.fs.chdir` or a
                // nested-call cwd override — persist into the next resume.
                let evolved = lua
                    .app_data_ref::<crate::ActiveContext>()
                    .map(|c| (*c).clone())
                    .unwrap_or_default();
                if status == ThreadStatus::Resumable {
                    // First yielded value should be our OpMarker userdata.
                    if let Some(first) = yielded.into_iter().next()
                        && let Value::UserData(ud) = first
                        && let Ok(marker) = ud.borrow::<OpMarker>()
                        && let Some(op) = marker.take()
                    {
                        return Ok((StepOutcome::Yielded(op), evolved));
                    }
                    return Err(mlua::Error::external(
                        "scheduler: coroutine yielded a non-op value",
                    ));
                }
                let json = multivalue_to_json(&lua, yielded)?;
                Ok((StepOutcome::Done(json), evolved))
            },
        )
        .await
        .map_err(DriveError::Join)?
        .map_err(DriveError::Lua)?;

        let (step, evolved) = step;
        // Carry cwd (and call_chain) mutations forward across the yield.
        ctx = evolved;
        match step {
            StepOutcome::Done(v) => return Ok(v),
            StepOutcome::Yielded(op) => {
                // Perform the IO outside the Lua mutex — this is the whole
                // point of the scheduler. Multiple coroutines can be in this
                // `.await` simultaneously; the mutex is free for everyone.
                let result = perform(op).await;
                next = match result {
                    Ok(v) => vec![v],
                    Err(msg) => vec![serde_json::json!({ "ok": false, "error": msg })],
                };
            }
        }
    }
}

// `Op` carries a full provider request in its `Ai` arm, so it dwarfs the
// `Done` value. Boxing would trade that for a heap alloc on every yield in the
// scheduler's hot path; the enum is short-lived and moved, so we keep it inline.
#[allow(clippy::large_enum_variant)]
enum StepOutcome {
    Done(serde_json::Value),
    Yielded(Op),
}

async fn perform(op: Op) -> Result<serde_json::Value, String> {
    match op {
        Op::Http(req) => match http_send(req).await {
            Ok(resp) => Ok(http_response_to_json(resp)),
            Err(e) => Err(e.to_string()),
        },
        Op::Shell(req) => match shell_exec(req).await {
            Ok(r) => Ok(shell_result_to_json(r)),
            Err(e) => Err(e.to_string()),
        },
        Op::Ai {
            provider_name,
            provider,
            request,
        } => match provider.complete(request).await {
            Ok(resp) => Ok(ai_response_to_json(provider_name, resp)),
            Err(e) => Err(e.to_string()),
        },
        Op::Sleep(d) => {
            tokio::time::sleep(d).await;
            Ok(serde_json::Value::Null)
        }
        Op::Await(state) => loop {
            if let Some(v) = state.read() {
                return v;
            }
            state.notify.notified().await;
        },
        Op::ChannelRecv(state) => {
            let rx = state.rx();
            let mut guard = rx.lock().await;
            match guard.recv().await {
                Some(v) => Ok(v),
                None => Err("channel closed".to_string()),
            }
        }
        Op::WsSendText { conn, msg } => conn
            .send_text(&msg)
            .await
            .map(|()| serde_json::Value::Null)
            .map_err(|e| e.to_string()),
        Op::WsSendBinary { conn, bytes } => conn
            .send_binary(bytes)
            .await
            .map(|()| serde_json::Value::Null)
            .map_err(|e| e.to_string()),
        Op::WsRecv { conn, timeout } => match conn.recv(timeout).await {
            Ok(frame) => Ok(ws_frame_to_json(frame)),
            Err(e) => Err(e.to_string()),
        },
        Op::WsClose { conn } => conn
            .close()
            .await
            .map(|()| serde_json::Value::Null)
            .map_err(|e| e.to_string()),
        Op::RunnerRun {
            dispatcher,
            caller,
            name,
            opts,
        } => dispatcher.run_runner_json(caller, &name, opts).await,
        Op::MailerSend { mailer, mail } => mailer
            .send(mail)
            .await
            .map(send_outcome_to_json)
            .map_err(|e| e.to_string()),
    }
}

fn send_outcome_to_json(o: SendOutcome) -> serde_json::Value {
    serde_json::json!({ "ok": true, "message_id": o.message_id })
}

fn ws_frame_to_json(frame: WsFrame) -> serde_json::Value {
    match frame {
        WsFrame::Text(s) => serde_json::json!({ "kind": "text", "text": s }),
        WsFrame::Binary(b) => {
            // Surface bytes as a `Vec<u8>` style array under `binary_bytes`;
            // most callers want JSON text frames anyway. If you need raw
            // bytes from Lua, ask: we can add a base64 mode later.
            serde_json::json!({ "kind": "binary", "binary_bytes": b })
        }
        WsFrame::Close { code, reason } => {
            serde_json::json!({ "kind": "close", "code": code, "reason": reason })
        }
    }
}

fn http_response_to_json(resp: HttpResponse) -> serde_json::Value {
    let mut headers = serde_json::Map::new();
    for (k, v) in resp.headers {
        headers.insert(k, serde_json::Value::String(v));
    }
    serde_json::json!({
        "status": resp.status,
        "body": resp.body,
        "headers": headers,
    })
}

fn shell_result_to_json(r: ExecResult) -> serde_json::Value {
    serde_json::json!({
        "exit_code": r.exit_code,
        "stdout": r.stdout,
        "stderr": r.stderr,
    })
}

fn ai_response_to_json(provider: String, r: CompletionResponse) -> serde_json::Value {
    serde_json::json!({
        "text": r.text,
        "model": r.model,
        "stop_reason": r.stop_reason,
        "provider": provider,
    })
}

fn json_to_multivalue(lua: &Lua, items: Vec<serde_json::Value>) -> mlua::Result<MultiValue> {
    use mlua::LuaSerdeExt;
    let mut mv = MultiValue::new();
    for v in items {
        mv.push_back(lua.to_value(&v)?);
    }
    Ok(mv)
}

fn multivalue_to_json(lua: &Lua, mv: MultiValue) -> mlua::Result<serde_json::Value> {
    use mlua::LuaSerdeExt;
    let collected: Vec<Value> = mv.into_iter().collect();
    if collected.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    if collected.len() == 1 {
        let only = collected.into_iter().next().unwrap();
        let v: serde_json::Value = lua.from_value(only)?;
        return Ok(v);
    }
    let mut arr: Vec<serde_json::Value> = Vec::new();
    for v in collected {
        let j: serde_json::Value = lua.from_value(v)?;
        arr.push(j);
    }
    Ok(serde_json::Value::Array(arr))
}

// ---------- Lua-side helpers ----------

/// True iff there is a currently-running coroutine that could legally call
/// `coroutine.yield`. Used by yieldable bindings to decide between yielding
/// and falling back to `block_on`.
pub(crate) fn is_in_coroutine(lua: &Lua) -> bool {
    let Ok(co_tbl) = lua.globals().get::<Table>("coroutine") else {
        return false;
    };
    let Ok(running) = co_tbl.get::<Function>("running") else {
        return false;
    };
    let Ok((_, is_main)) = running.call::<(Value, bool)>(()) else {
        return false;
    };
    !is_main
}

/// Build an `OpMarker` userdata that a yield-aware binding returns from its
/// C body. The accompanying Lua wrapper (`yieldable_wrap` in `lib.rs`) calls
/// `coroutine.yield(marker)` from a Lua frame — yielding from a Rust C frame
/// is illegal in Lua 5.4 ("attempt to yield across a C-call boundary"), so
/// we hand the marker out and let Lua do the yield.
pub(crate) fn build_marker(lua: &Lua, op: Op) -> mlua::Result<Value> {
    let ud = lua.create_userdata(OpMarker::new(op))?;
    Ok(Value::UserData(ud))
}

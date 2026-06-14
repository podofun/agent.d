//! Bare global `channel("name")` / `channel()` — the agentd actor-model
//! primitive. Replaces shared mutable state w/ message passing, sidestepping
//! the read-modify-write-across-yield window that's the only real race in
//! our cooperative scheduler.
//!
//! Implementation:
//!
//! - Underlying transport = `tokio::sync::mpsc::unbounded_channel<Value>`.
//!   Multi-producer, single-consumer; we serialize multiple Lua-side
//!   readers behind a `tokio::sync::Mutex` on the receiver.
//! - Messages pass as `serde_json::Value` — sender and receiver get
//!   independent copies, so a receiver can't mutate the sender's tables.
//!   Functions / userdata / threads can't be sent (they'd fail to
//!   serialize); pass plain data + names instead.
//! - Named channels (`channel("foo")`) are de-duplicated in a per-host
//!   `ChannelRegistry` so any service can grab the same channel by name.
//!   Anonymous channels (`channel()`) live only as long as their handle.
//! - The Lua-facing handle is a *table* (not raw userdata) so its `recv`
//!   method can be a Lua closure that performs `coroutine.yield` from a
//!   Lua frame — Lua 5.4 forbids yielding across C-call boundaries.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use mlua::{AnyUserData, Function, Lua, LuaSerdeExt, MultiValue, Table, Value};
use tokio::sync::{Mutex, mpsc};

use crate::scheduler;

/// Backing state for one channel. Cloneable handle (`Arc`); both ends share
/// the same `tx` and the same locked `rx`.
pub(crate) struct ChannelState {
    tx: mpsc::UnboundedSender<serde_json::Value>,
    rx: Arc<Mutex<mpsc::UnboundedReceiver<serde_json::Value>>>,
    closed: Arc<AtomicBool>,
}

impl ChannelState {
    fn new() -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub(crate) fn rx(&self) -> Arc<Mutex<mpsc::UnboundedReceiver<serde_json::Value>>> {
        self.rx.clone()
    }

    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

/// Process-wide registry of named channels. Anonymous channels never land
/// here; they live only on the stack of whoever opened them.
#[derive(Default, Clone)]
pub struct ChannelRegistry {
    inner: Arc<RwLock<HashMap<String, Arc<ChannelState>>>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn open_or_create(&self, name: &str) -> Arc<ChannelState> {
        if let Some(c) = self.inner.read().unwrap().get(name) {
            return c.clone();
        }
        let mut g = self.inner.write().unwrap();
        g.entry(name.to_string())
            .or_insert_with(ChannelState::new)
            .clone()
    }
}

/// Lua userdata wrapping the shared `ChannelState`. The user-facing handle
/// is a Lua *table* built by `install_channel_global`; this userdata is
/// stored under the table's `_state` field and consumed by the C internals.
struct ChannelHandle(Arc<ChannelState>);

impl mlua::UserData for ChannelHandle {}

/// Install the bare global `channel` plus the C internals it delegates to.
/// Returns nothing — the global is set directly.
pub(crate) fn install_channel_global(lua: &Lua) -> mlua::Result<()> {
    let registry = ChannelRegistry::new();
    lua.set_app_data(registry);

    let open = lua.create_function(channel_open_internal)?;
    let send = lua.create_function(channel_send_internal)?;
    let recv = lua.create_function(channel_recv_internal)?;
    let try_recv = lua.create_function(channel_try_recv_internal)?;
    let close = lua.create_function(channel_close_internal)?;
    let is_closed = lua.create_function(channel_is_closed_internal)?;

    // The Lua-side `channel(...)` constructor. Building the handle as a Lua
    // table (not userdata) lets `recv` be a Lua closure that does the
    // coroutine.yield — the same pattern the http/ai bindings use.
    let chunk_src = r#"
        local open, send, recv, try_recv, close, is_closed = ...
        return function(name)
          local state = open(name)
          return {
            _state = state,
            name = name,
            send = function(self, msg)
              return send(self._state, msg)
            end,
            recv = function(self)
              local r = recv(self._state)
              if type(r) == "userdata" then
                r = coroutine.yield(r)
                if type(r) == "table" and r.ok == false then
                  error(r.error or "channel error", 0)
                end
              end
              return r
            end,
            try_recv = function(self)
              return try_recv(self._state)
            end,
            close = function(self)
              return close(self._state)
            end,
            is_closed = function(self)
              return is_closed(self._state)
            end,
          }
        end
    "#;
    let constructor: Function = lua
        .load(chunk_src)
        .set_name("channel ctor")
        .call((open, send, recv, try_recv, close, is_closed))?;
    lua.globals().set("channel", constructor)?;
    Ok(())
}

fn channel_open_internal(lua: &Lua, args: MultiValue) -> mlua::Result<AnyUserData> {
    let mut iter = args.into_iter();
    let state = match iter.next() {
        Some(Value::Nil) | None => ChannelState::new(),
        Some(Value::String(s)) => {
            let name = s.to_str()?.to_string();
            let reg = lua
                .app_data_ref::<ChannelRegistry>()
                .ok_or_else(|| mlua::Error::external("channel registry missing"))?;
            reg.open_or_create(&name)
        }
        Some(other) => {
            return Err(mlua::Error::external(format!(
                "channel: name must be string or nil, got {}",
                other.type_name()
            )));
        }
    };
    lua.create_userdata(ChannelHandle(state))
}

fn channel_send_internal(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    let mut it = args.into_iter();
    let ud: AnyUserData = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("channel:send: state required"))?,
    )?;
    let msg = it
        .next()
        .ok_or_else(|| mlua::Error::external("channel:send: message required"))?;
    let payload: serde_json::Value = lua
        .from_value(msg)
        .map_err(|e| mlua::Error::external(format!("channel:send: serialize: {e}")))?;
    let handle = ud.borrow::<ChannelHandle>()?;
    if handle.0.closed() {
        return Err(mlua::Error::external("channel:send: channel is closed"));
    }
    handle
        .0
        .tx
        .send(payload)
        .map_err(|_| mlua::Error::external("channel:send: receiver dropped"))?;
    Ok(())
}

/// C internal: if the calling coroutine can yield, return an `OpMarker` for
/// the scheduler to await; otherwise block the current thread. The Lua
/// wrapper checks `type(r) == "userdata"` to know which case it's in.
fn channel_recv_internal(lua: &Lua, ud: AnyUserData) -> mlua::Result<Value> {
    let state = ud.borrow::<ChannelHandle>()?.0.clone();
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::ChannelRecv(state));
    }
    // Top-level fallback: block on a fresh future.
    let rx = state.rx();
    let handle_rt = tokio::runtime::Handle::try_current()
        .map_err(|e| mlua::Error::external(format!("channel:recv: no tokio runtime: {e}")))?;
    let received = handle_rt.block_on(async move {
        let mut guard = rx.lock().await;
        guard.recv().await
    });
    match received {
        Some(v) => Ok(lua.to_value(&v)?),
        None => Err(mlua::Error::external("channel:recv: channel closed")),
    }
}

fn channel_try_recv_internal(lua: &Lua, ud: AnyUserData) -> mlua::Result<Value> {
    let state = ud.borrow::<ChannelHandle>()?.0.clone();
    let rx = state.rx();
    let handle_rt = tokio::runtime::Handle::try_current()
        .map_err(|e| mlua::Error::external(format!("channel:try_recv: no tokio runtime: {e}")))?;
    let outcome = handle_rt.block_on(async move {
        let mut guard = rx.lock().await;
        guard.try_recv()
    });
    match outcome {
        Ok(v) => Ok(lua.to_value(&v)?),
        Err(mpsc::error::TryRecvError::Empty) => Ok(Value::Nil),
        Err(mpsc::error::TryRecvError::Disconnected) => {
            Err(mlua::Error::external("channel:try_recv: channel closed"))
        }
    }
}

fn channel_close_internal(_lua: &Lua, ud: AnyUserData) -> mlua::Result<()> {
    let state = ud.borrow::<ChannelHandle>()?.0.clone();
    state.closed.store(true, Ordering::SeqCst);
    Ok(())
}

fn channel_is_closed_internal(_lua: &Lua, ud: AnyUserData) -> mlua::Result<bool> {
    let state = ud.borrow::<ChannelHandle>()?.0.clone();
    Ok(state.closed())
}

// Allow the channel namespace to also expose `Table` for future extension
// without forcing every caller to import it.
#[allow(dead_code)]
fn _ensure_imports(_: &Table) {}

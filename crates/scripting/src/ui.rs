//! `agentd.ui`: scripts build each chat's UI from elements, the way a web
//! page is built with the DOM. The tree lives in [`agentd_ui::Hub`]; this
//! module gives Lua handles onto it and runs handlers with a `ctx` acting
//! for the chat.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agentd_permissions::{Caller, PermissionSet};
use agentd_shell::ExecRequest;
use agentd_ui::{
    CORE_ELEMENTS, Chat, ChatId, EntryDecoration, EntryInfo, Event, Hub, NodeId, Op, Props, parts,
};
use mlua::{
    AnyUserData, Function, Lua, LuaSerdeExt, MetaMethod, MultiValue, RegistryKey, Table, Thread,
    ThreadStatus, UserData, UserDataMethods, Value,
};

use crate::{ActiveContext, CtxTable, scheduler};

/// The grants UI handlers act with: those of `[tool.ui]`.
pub type UiGrants = Arc<dyn Fn() -> PermissionSet + Send + Sync>;

/// Set by the daemon: the hub every chat lives in, and the grants a chat's
/// `ctx` acts with.
#[derive(Clone, Default)]
pub(crate) struct UiLink(Option<(Arc<Hub>, UiGrants)>);

impl UiLink {
    pub(crate) fn new(hub: Arc<Hub>, grants: UiGrants) -> Self {
        Self(Some((hub, grants)))
    }
}

/// Present while `ui.frames` functions run ahead of time.
#[derive(Clone, Copy, Default)]
pub(crate) struct UiFrames(bool);

/// Frame functions, shared with the frame loop without the Lua lock.
type FrameIndex = Arc<Mutex<Vec<(u64, u32, Option<ChatId>)>>>;

struct FrameFn {
    id: u64,
    chat: Option<ChatId>,
    f: RegistryKey,
}

struct ListBinding {
    items: RegistryKey,
    render: RegistryKey,
    /// Key, content and element of each item as last drawn.
    drawn: Vec<(String, String, NodeId)>,
}

/// Everything scripts registered. Never borrowed across a call into Lua.
#[derive(Default)]
pub(crate) struct UiState {
    on: HashMap<String, Vec<RegistryKey>>,
    chat_on: HashMap<(ChatId, String), Vec<RegistryKey>>,
    element_on: HashMap<(ChatId, NodeId, String), Vec<RegistryKey>>,
    binds: Vec<(String, RegistryKey)>,
    components: HashMap<String, RegistryKey>,
    views: HashMap<String, RegistryKey>,
    frames: Vec<FrameFn>,
    frame_index: FrameIndex,
    next_frames: u64,
    terminals: HashMap<String, Vec<String>>,
    terminal_nodes: HashMap<(ChatId, NodeId), (u16, u16)>,
    lists: HashMap<(ChatId, NodeId), ListBinding>,
    entries: HashMap<(ChatId, String), EntryInfo>,
}

fn fail(message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(message.into())
}

/// Refuse anything but UI changes while frame functions run: they run
/// ahead of time, so their effects must not depend on when they run.
pub(crate) fn frames_guard(lua: &Lua) -> mlua::Result<()> {
    if lua.app_data_ref::<UiFrames>().is_some_and(|f| f.0) {
        return Err(fail(
            "only UI changes are allowed inside ui.frames, because frames are drawn ahead of time",
        ));
    }
    Ok(())
}

fn hub(lua: &Lua) -> mlua::Result<Arc<Hub>> {
    lua.app_data_ref::<UiLink>()
        .and_then(|l| l.0.as_ref().map(|(hub, _)| hub.clone()))
        .ok_or_else(|| fail("agentd.ui needs the daemon, so it is not available here"))
}

fn chat(lua: &Lua, id: ChatId) -> mlua::Result<Arc<Chat>> {
    hub(lua)?
        .chat(id)
        .ok_or_else(|| fail("this chat has closed"))
}

fn with_state<T>(lua: &Lua, f: impl FnOnce(&mut UiState) -> T) -> mlua::Result<T> {
    let mut state = lua.app_data_mut::<UiState>().ok_or_else(|| {
        fail("the UI state is missing from this Lua state, this is a bug in agentd")
    })?;
    Ok(f(&mut state))
}

fn keys_to_fns(lua: &Lua, keys: Option<&Vec<RegistryKey>>) -> Vec<Function> {
    keys.into_iter()
        .flatten()
        .filter_map(|k| lua.registry_value::<Function>(k).ok())
        .collect()
}

fn to_json(lua: &Lua, value: Value) -> mlua::Result<serde_json::Value> {
    if el_of(&value).is_some() {
        return Err(fail(
            "an element cannot be stored as a property, append it instead",
        ));
    }
    lua.from_value(value)
}

fn el_of(value: &Value) -> Option<El> {
    match value {
        Value::UserData(ud) => ud.borrow::<El>().ok().map(|e| *e),
        _ => None,
    }
}

fn need_el(value: &Value, chat: ChatId) -> mlua::Result<El> {
    let el = el_of(value).ok_or_else(|| fail("expected an element"))?;
    if el.chat != chat {
        return Err(fail("that element belongs to another chat"));
    }
    Ok(el)
}

fn ctx_table(lua: &Lua) -> mlua::Result<Table> {
    let key = lua
        .app_data_ref::<CtxTable>()
        .ok_or_else(|| fail("the shared `ctx` table is missing, this is a bug in agentd"))?;
    lua.registry_value(&key.0)
}

/// The `ctx` a chat's handlers act with: its interface, user, session and
/// folder, and the grants of `[tool.ui]` with `{cwd}` set to the folder.
fn chat_context(lua: &Lua, chat: &Chat) -> ActiveContext {
    let info = chat.info();
    let mut caller = Caller::interface(info.interface.as_str())
        .with_session(info.session.clone().unwrap_or(info.connection.clone()));
    if let Some(user) = &info.user {
        caller = caller.with_user(user.as_str());
    }
    caller.session_id = info.session.clone();
    caller.cwd = info.folder.clone();
    let granted = lua
        .app_data_ref::<UiLink>()
        .and_then(|l| l.0.as_ref().map(|(_, grants)| grants()))
        .unwrap_or_default();
    let cwd = info.folder.as_ref().map(PathBuf::from).or_else(|| {
        lua.app_data_ref::<crate::WorkspaceRoot>()
            .and_then(|w| w.0.clone())
    });
    ActiveContext {
        effective_grants: crate::normalize_path_grants(&crate::resolve_relative_grants(
            &crate::substitute_cwd(&granted, info.folder.as_deref()),
            cwd.as_deref(),
        )),
        call_chain: vec![format!("ui:{}", info.interface)],
        grant_kind: Some("tool".into()),
        grant_name: Some("ui".into()),
        caller,
        cwd,
        ..Default::default()
    }
}

/// A coroutine that calls `f(args..., ctx)`.
fn handler_thread(lua: &Lua, f: Function, args: Vec<Value>) -> mlua::Result<Thread> {
    let packed = lua.create_table()?;
    let n = args.len();
    for (i, a) in args.into_iter().enumerate() {
        packed.raw_set(i + 1, a)?;
    }
    packed.raw_set("n", n)?;
    let shim: Function = lua
        .load(
            "local f, ctx, args = ...\n\
             return function() args[args.n + 1] = ctx; return f(table.unpack(args, 1, args.n + 1)) end",
        )
        .set_name("ui handler")
        .call((f, ctx_table(lua)?, packed))?;
    lua.create_thread(shim)
}

fn chat_value(lua: &Lua, id: ChatId) -> mlua::Result<Value> {
    Ok(Value::UserData(lua.create_userdata(ChatUd(id))?))
}

fn el_value(lua: &Lua, chat: ChatId, node: NodeId) -> mlua::Result<Value> {
    Ok(Value::UserData(lua.create_userdata(El { chat, node })?))
}

/// Install `agentd.ui`.
pub(crate) fn install(lua: &Lua, agentd: &Table) -> mlua::Result<()> {
    lua.set_app_data(UiState::default());
    lua.set_app_data(UiLink::default());
    lua.set_app_data(UiFrames::default());
    let ui = lua.create_table()?;
    ui.set("on", lua.create_function(ui_on)?)?;
    ui.set("bind", lua.create_function(ui_bind)?)?;
    ui.set("chats", lua.create_function(ui_chats)?)?;
    ui.set("component", lua.create_function(ui_component)?)?;
    ui.set("view", lua.create_function(ui_view)?)?;
    ui.set(
        "frames",
        lua.create_function(|lua, (fps, f): (u32, Function)| add_frames(lua, fps, None, f))?,
    )?;
    ui.set("terminal", lua.create_function(ui_terminal)?)?;
    ui.set("data", lua.create_function(ui_data)?)?;
    let open = crate::yieldable_wrap(lua, lua.create_function(terminal_open_binding)?)?;
    let note = lua.create_function(|lua, (chat_id, text): (ChatId, String)| {
        if let Ok(chat) = chat(lua, chat_id) {
            chat.note(&format!("A terminal did not start. {text}"));
        }
        Ok(())
    })?;
    let opener: Function = lua
        .load(
            "local open, note = ...\n\
             return function(chat, node) return function()\n\
               local ok, err = pcall(open, chat, node)\n\
               if not ok then note(chat, tostring(err)) end\n\
             end end",
        )
        .set_name("ui terminal")
        .call((open, note))?;
    lua.set_named_registry_value("agentd.ui.terminal_opener", opener)?;
    agentd.set("ui", ui)?;
    Ok(())
}

const GLOBAL_EVENTS: [&str; 3] = ["chat", "session", "entry"];
const CHAT_EVENTS: [&str; 4] = ["session", "entry", "key", "resize"];

fn ui_on(lua: &Lua, (name, f): (String, Function)) -> mlua::Result<()> {
    if !GLOBAL_EVENTS.contains(&name.as_str()) {
        return Err(fail(format!(
            "agentd.ui.on takes `chat`, `session` or `entry`, not `{name}`"
        )));
    }
    let key = lua.create_registry_value(f)?;
    with_state(lua, |s| s.on.entry(name).or_default().push(key))
}

fn ui_bind(lua: &Lua, (key, f): (String, Function)) -> mlua::Result<()> {
    let key = agentd_ui::normalize_key(&key);
    if agentd_ui::is_reserved_key(&key) {
        return Err(fail(format!(
            "`{key}` belongs to chat, so scripts cannot bind it"
        )));
    }
    let f = lua.create_registry_value(f)?;
    let keys = with_state(lua, |s| {
        s.binds.push((key, f));
        bound_keys(s)
    })?;
    if let Ok(hub) = hub(lua) {
        for chat in hub.chats() {
            chat.doc().set_bindings(keys.clone());
        }
    }
    Ok(())
}

fn bound_keys(state: &UiState) -> Vec<String> {
    let set: BTreeSet<String> = state.binds.iter().map(|(k, _)| k.clone()).collect();
    set.into_iter().collect()
}

fn ui_chats(lua: &Lua, _: ()) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    for (i, chat) in hub(lua)?.chats().into_iter().enumerate() {
        out.raw_set(i + 1, chat_value(lua, chat.id)?)?;
    }
    Ok(out)
}

fn ui_component(lua: &Lua, (name, f): (String, Function)) -> mlua::Result<()> {
    if CORE_ELEMENTS.contains(&name.as_str()) || name.starts_with("chat.") {
        return Err(fail(format!(
            "`{name}` is a built-in element, pick another name"
        )));
    }
    let key = lua.create_registry_value(f)?;
    with_state(lua, |s| s.components.insert(name, key))?;
    Ok(())
}

fn ui_view(lua: &Lua, (name, f): (String, Function)) -> mlua::Result<()> {
    if name == "chat" {
        return Err(fail(
            "the `chat` view is chat's own, change it through its elements instead",
        ));
    }
    let key = lua.create_registry_value(f)?;
    with_state(lua, |s| s.views.insert(name, key))?;
    Ok(())
}

fn add_frames(lua: &Lua, fps: u32, chat: Option<ChatId>, f: Function) -> mlua::Result<AnyUserData> {
    if !(1..=60).contains(&fps) {
        return Err(fail("ui.frames takes 1 to 60 frames per second"));
    }
    let f = lua.create_registry_value(f)?;
    let id = with_state(lua, |s| {
        s.next_frames += 1;
        let id = s.next_frames;
        s.frames.push(FrameFn { id, chat, f });
        s.frame_index.lock().unwrap().push((id, fps, chat));
        id
    })?;
    lua.create_userdata(FramesUd(id))
}

fn stop_frames(lua: &Lua, id: u64) -> mlua::Result<()> {
    with_state(lua, |s| {
        s.frames.retain(|f| f.id != id);
        s.frame_index.lock().unwrap().retain(|(i, _, _)| *i != id);
    })
}

fn ui_terminal(lua: &Lua, (name, argv): (String, Vec<String>)) -> mlua::Result<()> {
    if argv.is_empty() {
        return Err(fail(format!(
            "the terminal `{name}` needs a program to run, such as {{ \"htop\" }}"
        )));
    }
    with_state(lua, |s| s.terminals.insert(name, argv))?;
    Ok(())
}

/// `ui.data(t)`: a table that redraws the lists bound to it when written.
fn ui_data(lua: &Lua, initial: Option<Table>) -> mlua::Result<Table> {
    let backing = lua.create_table()?;
    if let Some(t) = initial {
        for pair in t.pairs::<Value, Value>() {
            let (k, v) = pair?;
            backing.raw_set(k, v)?;
        }
    }
    let proxy = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", backing.clone())?;
    let len_of = backing.clone();
    meta.set(
        "__len",
        lua.create_function(move |_, _: Value| Ok(len_of.raw_len()))?,
    )?;
    let write = backing.clone();
    meta.set(
        "__newindex",
        lua.create_function(move |lua, (proxy, k, v): (Table, Value, Value)| {
            write.raw_set(k, v)?;
            touch(lua, &proxy)
        })?,
    )?;
    let walk = backing.clone();
    meta.set(
        "__pairs",
        lua.create_function(move |lua, _: Value| {
            let next: Function = lua.globals().get("next")?;
            Ok((next, walk.clone(), Value::Nil))
        })?,
    )?;
    proxy.set_metatable(Some(meta))?;
    Ok(proxy)
}

/// Redraw every list bound to `items`.
fn touch(lua: &Lua, items: &Table) -> mlua::Result<()> {
    let bound: Vec<(ChatId, NodeId)> = with_state(lua, |s| {
        s.lists
            .iter()
            .filter(|(_, b)| {
                lua.registry_value::<Table>(&b.items)
                    .is_ok_and(|t| t == *items)
            })
            .map(|(k, _)| *k)
            .collect()
    })?;
    for (chat, node) in bound {
        refresh_list(lua, chat, node)?;
    }
    Ok(())
}

fn item_key(lua: &Lua, item: &Value, index: usize) -> String {
    let named = |t: &Table, field: &str| {
        t.get::<Value>(field)
            .ok()
            .and_then(|v| lua.coerce_string(v).ok().flatten())
            .map(|s| s.to_string_lossy())
    };
    match item {
        Value::Table(t) => named(t, "id")
            .or_else(|| named(t, "key"))
            .unwrap_or_else(|| format!("#{index}")),
        Value::String(s) => s.to_string_lossy(),
        Value::Integer(n) => n.to_string(),
        Value::Number(n) => n.to_string(),
        _ => format!("#{index}"),
    }
}

/// Redraw a bound list, rendering only the items that are new or changed.
fn refresh_list(lua: &Lua, chat_id: ChatId, node: NodeId) -> mlua::Result<()> {
    let Some((items, render, drawn)) = with_state(lua, |s| {
        s.lists.get(&(chat_id, node)).map(|b| {
            (
                lua.registry_value::<Table>(&b.items),
                lua.registry_value::<Function>(&b.render),
                b.drawn.clone(),
            )
        })
    })?
    else {
        return Ok(());
    };
    let (items, render) = (items?, render?);
    let chat = chat(lua, chat_id)?;
    let mut old: HashMap<String, (String, NodeId)> =
        drawn.into_iter().map(|(k, j, n)| (k, (j, n))).collect();
    let mut next: Vec<(String, String, NodeId)> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stale = Vec::new();
    let len = items.len()? as usize;
    for i in 1..=len {
        let item: Value = items.get(i)?;
        let mut key = item_key(lua, &item, i);
        while !seen.insert(key.clone()) {
            key.push('+');
        }
        let json = lua
            .from_value::<serde_json::Value>(item.clone())
            .map(|v| v.to_string())
            .unwrap_or_else(|_| format!("{item:?}"));
        match old.remove(&key) {
            Some((before, n)) if before == json && chat.doc().exists(n) => {
                next.push((key, json, n));
                continue;
            }
            Some((_, n)) => stale.push(n),
            None => {}
        }
        let el: Value = render.call((item, i, chat_value(lua, chat_id)?))?;
        let el = need_el(&el, chat_id)
            .map_err(|_| fail("a list's render function must return an element"))?;
        next.push((key, json, el.node));
    }
    stale.extend(old.into_values().map(|(_, n)| n));
    {
        let mut doc = chat.doc();
        for n in stale {
            doc.destroy(n);
        }
        let mut current = doc.children(node);
        for (index, (_, _, n)) in next.iter().enumerate() {
            if current.get(index) != Some(n) {
                doc.insert(node, *n, index).map_err(fail)?;
                current = doc.children(node);
            }
        }
    }
    with_state(lua, |s| {
        if let Some(b) = s.lists.get_mut(&(chat_id, node)) {
            b.drawn = next;
        }
    })
}

/// Create an element (or a component) in a chat's document.
fn create(lua: &Lua, chat_id: ChatId, kind: &str, props: Option<Table>) -> mlua::Result<Value> {
    frames_guard_for_structure(lua)?;
    let component = with_state(lua, |s| {
        s.components
            .get(kind)
            .and_then(|k| lua.registry_value::<Function>(k).ok())
    })?;
    if let Some(component) = component {
        let props = match props {
            Some(p) => p,
            None => lua.create_table()?,
        };
        let el: Value = component.call((props, chat_value(lua, chat_id)?))?;
        need_el(&el, chat_id)
            .map_err(|_| fail(format!("the component `{kind}` must return an element")))?;
        return Ok(el);
    }
    if !CORE_ELEMENTS.contains(&kind) && !kind.contains('.') || kind.starts_with("chat.") {
        return Err(fail(format!(
            "there is no element or component named `{kind}`"
        )));
    }
    let chat = chat(lua, chat_id)?;
    let mut p = Props::new();
    let mut style = Props::new();
    let mut children = Vec::new();
    let mut handlers = Vec::new();
    let mut items = None;
    let mut render = None;
    if let Some(t) = props {
        for pair in t.pairs::<String, Value>() {
            let (key, value) = pair?;
            match (key.as_str(), value) {
                ("children", Value::Table(list)) => {
                    for child in list.sequence_values::<Value>() {
                        children.push(need_el(&child?, chat_id)?.node);
                    }
                }
                ("style", Value::Table(s)) => {
                    for pair in s.pairs::<String, Value>() {
                        let (k, v) = pair?;
                        style.insert(k, to_json(lua, v)?);
                    }
                }
                ("on", Value::Table(on)) => {
                    for pair in on.pairs::<String, Function>() {
                        handlers.push(pair?);
                    }
                }
                ("items", Value::Table(t)) if kind == "list" => items = Some(t),
                ("render", Value::Function(f)) if kind == "list" => render = Some(f),
                (_, value) => {
                    p.insert(key, to_json(lua, value)?);
                }
            }
        }
    }
    if kind == "terminal" {
        let name = p.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        let known = with_state(lua, |s| s.terminals.contains_key(name))?;
        if !known {
            return Err(fail(format!(
                "there is no terminal named `{name}`, declare it with agentd.ui.terminal first"
            )));
        }
    }
    let node = {
        let mut doc = chat.doc();
        let node = doc.create(kind, p, style);
        for (i, child) in children.into_iter().enumerate() {
            doc.insert(node, child, i).map_err(fail)?;
        }
        node
    };
    for (event, f) in handlers {
        add_handler(lua, chat_id, node, event, f)?;
    }
    if let Some(items) = items {
        let render =
            render.ok_or_else(|| fail("a list bound to items needs a `render` function"))?;
        bind_list(lua, chat_id, node, items, render)?;
    }
    if kind == "terminal" {
        with_state(lua, |s| s.terminal_nodes.insert((chat_id, node), (80, 24)))?;
        open_terminal(lua, &chat, node)?;
    }
    el_value(lua, chat_id, node)
}

/// Frame functions may change what elements show, not build new ones.
fn frames_guard_for_structure(lua: &Lua) -> mlua::Result<()> {
    if lua.app_data_ref::<UiFrames>().is_some_and(|f| f.0) {
        return Err(fail(
            "ui.frames functions may change elements but not create them, build them beforehand",
        ));
    }
    Ok(())
}

fn bind_list(
    lua: &Lua,
    chat: ChatId,
    node: NodeId,
    items: Table,
    render: Function,
) -> mlua::Result<()> {
    let items = lua.create_registry_value(items)?;
    let render = lua.create_registry_value(render)?;
    let drawn = with_state(lua, |s| {
        s.lists
            .remove(&(chat, node))
            .map(|b| b.drawn)
            .unwrap_or_default()
    })?;
    with_state(lua, |s| {
        s.lists.insert(
            (chat, node),
            ListBinding {
                items,
                render,
                drawn,
            },
        )
    })?;
    refresh_list(lua, chat, node)
}

fn add_handler(
    lua: &Lua,
    chat: ChatId,
    node: NodeId,
    event: String,
    f: Function,
) -> mlua::Result<()> {
    const EVENTS: [&str; 6] = ["click", "key", "change", "focus", "blur", "hover"];
    if !EVENTS.contains(&event.as_str()) {
        return Err(fail(format!(
            "elements raise click, key, change, focus, blur and hover, not `{event}`"
        )));
    }
    let key = lua.create_registry_value(f)?;
    with_state(lua, |s| {
        s.element_on
            .entry((chat, node, event))
            .or_default()
            .push(key)
    })
}

/// Start the program of a `terminal` element in the background, asking for
/// approval like `ctx.shell` would.
fn open_terminal(lua: &Lua, chat: &Chat, node: NodeId) -> mlua::Result<()> {
    let opener: Function = lua.named_registry_value("agentd.ui.terminal_opener")?;
    let body: Function = opener.call((chat.id, node))?;
    let thread = lua.create_thread(body)?;
    let spawner = lua
        .app_data_ref::<scheduler::AsyncTaskSpawner>()
        .map(|s| s.0.clone())
        .ok_or_else(|| fail("terminal panes need the daemon's async runtime"))?;
    let _ = spawner.send(scheduler::AsyncTask {
        thread,
        state: scheduler::AsyncHandleState::new(),
        ctx: chat_context(lua, chat),
    });
    Ok(())
}

fn terminal_open_binding(lua: &Lua, (chat_id, node): (ChatId, NodeId)) -> mlua::Result<()> {
    let chat = chat(lua, chat_id)?;
    if chat.has_terminal(node) {
        return Ok(());
    }
    let name = chat
        .doc()
        .prop(node, "name")
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default();
    let (argv, size) = with_state(lua, |s| {
        (
            s.terminals.get(&name).cloned(),
            s.terminal_nodes.get(&(chat_id, node)).copied(),
        )
    })?;
    let argv = argv.ok_or_else(|| fail(format!("there is no terminal named `{name}`")))?;
    let Some(folder) = chat.info().folder else {
        return Err(fail(
            "this chat has no folder yet, so it opens once the chat has one",
        ));
    };
    let mut req = ExecRequest {
        bin: argv[0].clone(),
        args: argv[1..].to_vec(),
        cwd: Some(PathBuf::from(folder)),
        stdin: None,
        separate_stderr: false,
        timeout_ms: None,
        sandbox: None,
    };
    crate::prepare_exec(lua, &mut req)?;
    let (width, height) = size.unwrap_or((80, 24));
    let pty = agentd_shell::spawn_pty(&req, width, height).map_err(|e| fail(e.to_string()))?;
    chat.open_terminal(
        node,
        agentd_ui::TerminalIo {
            reader: pty.reader,
            writer: pty.writer,
            resize: pty.resize,
            keep: pty.child,
        },
        width,
        height,
    );
    Ok(())
}

/// Switch a chat to a view, building it the first time.
fn show(lua: &Lua, chat_id: ChatId, name: &str) -> mlua::Result<()> {
    let chat = chat(lua, chat_id)?;
    if chat.doc().view_root(name).is_none() {
        let builder = with_state(lua, |s| {
            s.views
                .get(name)
                .and_then(|k| lua.registry_value::<Function>(k).ok())
        })?
        .ok_or_else(|| {
            fail(format!(
                "there is no view named `{name}`, register it with agentd.ui.view"
            ))
        })?;
        let root: Value = builder.call((chat_value(lua, chat_id)?, ctx_table(lua)?))?;
        let root = need_el(&root, chat_id)
            .map_err(|_| fail(format!("the view `{name}` must return an element")))?;
        chat.doc().add_view(name, root.node);
    }
    chat.doc().show(name).map_err(fail)
}

/// A chat, as handlers see it.
#[derive(Clone, Copy)]
pub(crate) struct ChatUd(pub(crate) ChatId);

impl UserData for ChatUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("on", |lua, this, (name, f): (String, Function)| {
            if !CHAT_EVENTS.contains(&name.as_str()) {
                return Err(fail(format!(
                    "a chat raises session, entry, key and resize, not `{name}`"
                )));
            }
            let key = lua.create_registry_value(f)?;
            with_state(lua, |s| {
                s.chat_on.entry((this.0, name)).or_default().push(key)
            })
        });
        methods.add_method("show", |lua, this, name: String| show(lua, this.0, &name));
        methods.add_method("frames", |lua, this, (fps, f): (u32, Function)| {
            add_frames(lua, fps, Some(this.0), f)
        });
        methods.add_method("note", |lua, this, text: String| {
            chat(lua, this.0)?.note(&text);
            Ok(())
        });
        methods.add_method("entry", |lua, this, key: String| {
            let known = with_state(lua, |s| s.entries.contains_key(&(this.0, key.clone())))?;
            if !known {
                return Ok(Value::Nil);
            }
            Ok(Value::UserData(
                lua.create_userdata(EntryUd { chat: this.0, key })?,
            ))
        });
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            let chat = chat(lua, this.0)?;
            let info = chat.info();
            let text = |s: Option<String>| -> mlua::Result<Value> {
                Ok(match s {
                    Some(s) => Value::String(lua.create_string(&s)?),
                    None => Value::Nil,
                })
            };
            match key.as_str() {
                "id" => Ok(Value::Integer(this.0 as i64)),
                "session" => text(info.session),
                "folder" => text(info.folder),
                "user" => text(info.user),
                "interface" => text(Some(info.interface)),
                "view" => text(Some(chat.doc().view().to_string())),
                "document" => Ok(Value::UserData(lua.create_userdata(DocUd(this.0))?)),
                "client" => {
                    let t = lua.create_table()?;
                    t.set("kind", info.client.kind.clone())?;
                    t.set("width", info.client.width)?;
                    t.set("height", info.client.height)?;
                    t.set("extras", info.client.supports.clone())?;
                    let extras = info.client.supports.clone();
                    t.set(
                        "supports",
                        lua.create_function(move |_, name: String| {
                            Ok(CORE_ELEMENTS.contains(&name.as_str()) || extras.contains(&name))
                        })?,
                    )?;
                    Ok(Value::Table(t))
                }
                _ => Ok(Value::Nil),
            }
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (String, Value)| match (key.as_str(), value) {
                ("view", Value::String(name)) => show(lua, this.0, &name.to_str()?),
                _ => Err(fail(format!("a chat's `{key}` cannot be changed"))),
            },
        );
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            Ok(other.borrow::<ChatUd>().is_ok_and(|o| o.0 == this.0))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("chat {}", this.0))
        });
    }
}

/// A chat's document: where its elements are made and found.
#[derive(Clone, Copy)]
struct DocUd(ChatId);

impl UserData for DocUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "create",
            |lua, this, (kind, props): (String, Option<Table>)| create(lua, this.0, &kind, props),
        );
        for kind in CORE_ELEMENTS {
            methods.add_method(kind, move |lua, this, props: Option<Table>| {
                create(lua, this.0, kind, props)
            });
        }
        methods.add_method("get", |lua, this, id: String| {
            match chat(lua, this.0)?.doc().find(&id) {
                Some(node) => el_value(lua, this.0, node),
                None => Ok(Value::Nil),
            }
        });
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            if key == "root" {
                return match chat(lua, this.0)?.doc().root() {
                    Some(node) => el_value(lua, this.0, node),
                    None => Ok(Value::Nil),
                };
            }
            let known = with_state(lua, |s| s.components.contains_key(&key))?;
            if !known {
                return Ok(Value::Nil);
            }
            let chat = this.0;
            Ok(Value::Function(lua.create_function(
                move |lua, (_doc, props): (Value, Option<Table>)| create(lua, chat, &key, props),
            )?))
        });
    }
}

/// An element in a chat's document.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct El {
    chat: ChatId,
    node: NodeId,
}

fn guard_part(node: NodeId, what: &str) -> mlua::Result<()> {
    if node == parts::PROMPT {
        return Err(fail(format!(
            "the prompt belongs to chat, so scripts cannot {what} it"
        )));
    }
    Ok(())
}

impl El {
    fn insert(&self, lua: &Lua, child: Value, index: Option<usize>) -> mlua::Result<()> {
        frames_guard_for_structure(lua)?;
        let child = need_el(&child, self.chat)?;
        let chat = chat(lua, self.chat)?;
        let mut doc = chat.doc();
        let index = index.unwrap_or_else(|| doc.children(self.node).len());
        doc.insert(self.node, child.node, index).map_err(fail)
    }

    fn get(&self, lua: &Lua, key: &str) -> mlua::Result<Value> {
        let chat = chat(lua, self.chat)?;
        let doc = chat.doc();
        if !doc.exists(self.node) {
            return Ok(Value::Nil);
        }
        match key {
            "kind" => Ok(Value::String(
                lua.create_string(doc.kind(self.node).unwrap_or_default())?,
            )),
            "parent" => match doc.parent(self.node) {
                Some(p) => el_value(lua, self.chat, p),
                None => Ok(Value::Nil),
            },
            "children" => {
                let t = lua.create_table()?;
                for (i, c) in doc.children(self.node).into_iter().enumerate() {
                    t.raw_set(i + 1, el_value(lua, self.chat, c)?)?;
                }
                Ok(Value::Table(t))
            }
            "style" => Ok(Value::UserData(lua.create_userdata(StyleUd {
                chat: self.chat,
                target: Target::Node(self.node),
            })?)),
            "items" => {
                drop(doc);
                with_state(lua, |s| {
                    s.lists
                        .get(&(self.chat, self.node))
                        .and_then(|b| lua.registry_value::<Value>(&b.items).ok())
                        .unwrap_or(Value::Nil)
                })
            }
            _ => match doc.prop(self.node, key) {
                Some(v) => lua.to_value(v),
                None => Ok(Value::Nil),
            },
        }
    }

    fn set(&self, lua: &Lua, key: &str, value: Value) -> mlua::Result<()> {
        match (key, value) {
            ("kind" | "parent" | "children", _) => {
                Err(fail(format!("an element's `{key}` cannot be assigned")))
            }
            ("style", Value::Table(t)) => {
                let style = StyleUd {
                    chat: self.chat,
                    target: Target::Node(self.node),
                };
                for pair in t.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    style.set(lua, &k, v)?;
                }
                Ok(())
            }
            ("items", Value::Table(items)) => {
                let render = with_state(lua, |s| {
                    s.lists
                        .get(&(self.chat, self.node))
                        .and_then(|b| lua.registry_value::<Function>(&b.render).ok())
                })?
                .ok_or_else(|| fail("set the list's `render` function before its items"))?;
                bind_list(lua, self.chat, self.node, items, render)
            }
            ("render", Value::Function(render)) => {
                let items = with_state(lua, |s| {
                    s.lists
                        .get(&(self.chat, self.node))
                        .and_then(|b| lua.registry_value::<Table>(&b.items).ok())
                })?;
                let items = match items {
                    Some(t) => t,
                    None => lua.create_table()?,
                };
                bind_list(lua, self.chat, self.node, items, render)
            }
            ("visible", Value::Boolean(false)) if self.node == parts::PROMPT => {
                guard_part(self.node, "hide")
            }
            (key, value) => {
                let value = to_json(lua, value)?;
                chat(lua, self.chat)?.doc().set(self.node, key, value);
                Ok(())
            }
        }
    }

    fn destroy(&self, lua: &Lua) -> mlua::Result<()> {
        guard_part(self.node, "remove")?;
        let chat = chat(lua, self.chat)?;
        chat.doc().destroy(self.node);
        let gone: Vec<NodeId> = with_state(lua, |s| {
            let doc = chat.doc();
            let gone: Vec<NodeId> = s
                .terminal_nodes
                .keys()
                .filter(|(c, n)| *c == self.chat && !doc.exists(*n))
                .map(|(_, n)| *n)
                .collect();
            s.element_on
                .retain(|(c, n, _), _| *c != self.chat || doc.exists(*n));
            s.lists
                .retain(|(c, n), _| *c != self.chat || doc.exists(*n));
            s.terminal_nodes
                .retain(|(c, n), _| *c != self.chat || doc.exists(*n));
            gone
        })?;
        for node in gone {
            chat.close_terminal(node);
        }
        Ok(())
    }
}

impl UserData for El {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("append", |lua, this, child: Value| {
            this.insert(lua, child, None)
        });
        methods.add_method("prepend", |lua, this, child: Value| {
            this.insert(lua, child, Some(0))
        });
        methods.add_method("insert", |lua, this, (child, index): (Value, usize)| {
            this.insert(lua, child, Some(index.saturating_sub(1)))
        });
        methods.add_method("remove", |lua, this, ()| {
            frames_guard_for_structure(lua)?;
            guard_part(this.node, "remove")?;
            chat(lua, this.chat)?.doc().remove(this.node);
            Ok(())
        });
        methods.add_method("destroy", |lua, this, ()| {
            frames_guard_for_structure(lua)?;
            this.destroy(lua)
        });
        methods.add_method("on", |lua, this, (event, f): (String, Function)| {
            add_handler(lua, this.chat, this.node, event, f)
        });
        methods.add_method("off", |lua, this, event: String| {
            with_state(lua, |s| s.element_on.remove(&(this.chat, this.node, event)))?;
            Ok(())
        });
        methods.add_method("find", |lua, this, id: String| {
            match chat(lua, this.chat)?.doc().find_within(this.node, &id) {
                Some(node) => el_value(lua, this.chat, node),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("refresh", |lua, this, ()| {
            refresh_list(lua, this.chat, this.node)
        });
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            this.get(lua, &key)
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (String, Value)| this.set(lua, &key, value),
        );
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            Ok(other.borrow::<El>().is_ok_and(|o| *o == *this))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("element {}", this.node))
        });
    }
}

#[derive(Clone)]
enum Target {
    Node(NodeId),
    Entry(String),
}

/// `el.style` or `entry.style`: plain assignment sets one style.
#[derive(Clone)]
struct StyleUd {
    chat: ChatId,
    target: Target,
}

impl StyleUd {
    fn set(&self, lua: &Lua, key: &str, value: Value) -> mlua::Result<()> {
        let value = to_json(lua, value)?;
        let chat = chat(lua, self.chat)?;
        let mut doc = chat.doc();
        match &self.target {
            Target::Node(node) => doc.set_style(*node, key, value),
            Target::Entry(entry) => {
                let mut deco = doc.decoration(entry);
                if value.is_null() {
                    deco.style.remove(key);
                } else {
                    deco.style.insert(key.to_string(), value);
                }
                doc.decorate(entry, deco);
            }
        }
        Ok(())
    }
}

impl UserData for StyleUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            let chat = chat(lua, this.chat)?;
            let doc = chat.doc();
            let value = match &this.target {
                Target::Node(node) => doc.style(*node, &key).cloned(),
                Target::Entry(entry) => doc.decoration(entry).style.get(&key).cloned(),
            };
            match value {
                Some(v) => lua.to_value(&v),
                None => Ok(Value::Nil),
            }
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (String, Value)| this.set(lua, &key, value),
        );
    }
}

/// A conversation entry: read its details, restyle, hide or summarize it,
/// and attach elements around it.
#[derive(Clone)]
struct EntryUd {
    chat: ChatId,
    key: String,
}

impl EntryUd {
    fn decorate(&self, lua: &Lua, f: impl FnOnce(&mut EntryDecoration)) -> mlua::Result<()> {
        let chat = chat(lua, self.chat)?;
        let mut doc = chat.doc();
        let mut deco = doc.decoration(&self.key);
        f(&mut deco);
        doc.decorate(&self.key, deco);
        Ok(())
    }
}

impl UserData for EntryUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("attach_before", |lua, this, el: Value| {
            let el = need_el(&el, this.chat)?;
            this.decorate(lua, |d| d.before.push(el.node))
        });
        methods.add_method("attach_after", |lua, this, el: Value| {
            let el = need_el(&el, this.chat)?;
            this.decorate(lua, |d| d.after.push(el.node))
        });
        methods.add_method("detach", |lua, this, ()| {
            this.decorate(lua, |d| {
                d.before.clear();
                d.after.clear();
            })
        });
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            let info = with_state(lua, |s| {
                s.entries.get(&(this.chat, this.key.clone())).cloned()
            })?
            .unwrap_or_default();
            let deco = chat(lua, this.chat)?.doc().decoration(&this.key);
            let json = |v: &Option<serde_json::Value>| match v {
                Some(v) => lua.to_value(v),
                None => Ok(Value::Nil),
            };
            let text = |s: Option<String>| -> mlua::Result<Value> {
                Ok(match s {
                    Some(s) => Value::String(lua.create_string(&s)?),
                    None => Value::Nil,
                })
            };
            match key.as_str() {
                "key" => text(Some(info.key)),
                "kind" => text(Some(info.kind)),
                "text" => text(info.text),
                "name" => text(info.name),
                "input" => json(&info.input),
                "result" => json(&info.result),
                "failed" => Ok(Value::Boolean(info.failed)),
                "changes" => lua.to_value(&info.changes),
                "summary" => text(deco.summary),
                "hidden" => Ok(Value::Boolean(deco.hidden)),
                "style" => Ok(Value::UserData(lua.create_userdata(StyleUd {
                    chat: this.chat,
                    target: Target::Entry(this.key.clone()),
                })?)),
                _ => Ok(Value::Nil),
            }
        });
        methods.add_meta_method(MetaMethod::NewIndex, |lua, this, (key, value): (String, Value)| {
            match (key.as_str(), value) {
                ("summary", Value::Nil) => this.decorate(lua, |d| d.summary = None),
                ("summary", Value::String(s)) => {
                    let s = agentd_ui::sanitize(&s.to_str()?);
                    let line = s.lines().next().unwrap_or_default().to_string();
                    this.decorate(lua, |d| d.summary = Some(line))
                }
                ("hidden", Value::Boolean(b)) => this.decorate(lua, |d| d.hidden = b),
                ("style", Value::Table(t)) => {
                    let style = StyleUd {
                        chat: this.chat,
                        target: Target::Entry(this.key.clone()),
                    };
                    for pair in t.pairs::<String, Value>() {
                        let (k, v) = pair?;
                        style.set(lua, &k, v)?;
                    }
                    Ok(())
                }
                _ => Err(fail(format!(
                    "an entry's `{key}` cannot be changed, scripts may set its summary, hidden and style"
                ))),
            }
        });
    }
}

struct FramesUd(u64);

impl UserData for FramesUd {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("stop", |lua, this, ()| stop_frames(lua, this.0));
    }
}

/// What a chat's handlers are asked to do.
enum Job {
    Opened,
    Session,
    Entry(EntryInfo),
    Event(Option<NodeId>, Event),
}

/// The scripts' side of [`agentd_ui::Hub`], backed by a Lua state.
pub(crate) struct LuaUi {
    pub(crate) lua: Arc<Mutex<Lua>>,
    pub(crate) frames: FrameIndex,
    pub(crate) runtime: tokio::runtime::Handle,
}

pub(crate) fn frame_index(lua: &Lua) -> FrameIndex {
    lua.app_data_ref::<UiState>()
        .map(|s| s.frame_index.clone())
        .unwrap_or_default()
}

impl LuaUi {
    fn run(&self, chat: Arc<Chat>, job: Job) {
        let lua = self.lua.clone();
        self.runtime.spawn(async move {
            let prepared = {
                let (lua, chat) = (lua.clone(), chat.clone());
                tokio::task::spawn_blocking(move || {
                    let lua = lua.lock().unwrap();
                    let threads = prepare(&lua, &chat, job);
                    (threads, chat_context(&lua, &chat))
                })
                .await
            };
            let Ok((threads, ctx)) = prepared else { return };
            let threads = match threads {
                Ok(t) => t,
                Err(e) => return report(&chat, &e.to_string()),
            };
            for thread in threads {
                if let Err(e) = scheduler::drive(lua.clone(), thread, vec![], ctx.clone()).await {
                    report(&chat, &e.to_string());
                }
            }
        });
    }
}

/// A script error: a note in the chat and a line in the log. The rest of
/// the UI keeps working.
fn report(chat: &Chat, raw: &str) {
    let (message, _) = crate::luaerr::split_lua_error(raw);
    tracing::warn!(chat = chat.id, error = %message, "a UI script failed");
    chat.note(&format!("A UI script failed. {message}"));
}

fn prepare(lua: &Lua, chat: &Arc<Chat>, job: Job) -> mlua::Result<Vec<Thread>> {
    let id = chat.id;
    let chat_v = chat_value(lua, id)?;
    let mut calls: Vec<(Function, Vec<Value>)> = Vec::new();
    match job {
        Job::Opened => {
            let (keys, fns) =
                with_state(lua, |s| (bound_keys(s), keys_to_fns(lua, s.on.get("chat"))))?;
            chat.doc().set_bindings(keys);
            calls.extend(fns.into_iter().map(|f| (f, vec![chat_v.clone()])));
        }
        Job::Session => {
            let (fns, terminals) = with_state(lua, |s| {
                s.entries.retain(|(c, _), _| *c != id);
                let mut fns = keys_to_fns(lua, s.on.get("session"));
                fns.extend(keys_to_fns(lua, s.chat_on.get(&(id, "session".into()))));
                let terminals: Vec<NodeId> = s
                    .terminal_nodes
                    .keys()
                    .filter(|(c, _)| *c == id)
                    .map(|(_, n)| *n)
                    .collect();
                (fns, terminals)
            })?;
            calls.extend(fns.into_iter().map(|f| (f, vec![chat_v.clone()])));
            if chat.info().folder.is_some() {
                for node in terminals {
                    if !chat.has_terminal(node) {
                        open_terminal(lua, chat, node)?;
                    }
                }
            }
        }
        Job::Entry(entry) => {
            let key = entry.key.clone();
            let fns = with_state(lua, |s| {
                s.entries.insert((id, key.clone()), entry);
                let mut fns = keys_to_fns(lua, s.on.get("entry"));
                fns.extend(keys_to_fns(lua, s.chat_on.get(&(id, "entry".into()))));
                fns
            })?;
            let entry_v = Value::UserData(lua.create_userdata(EntryUd { chat: id, key })?);
            calls.extend(
                fns.into_iter()
                    .map(|f| (f, vec![entry_v.clone(), chat_v.clone()])),
            );
        }
        Job::Event(node, event) => {
            let ev = lua.create_table()?;
            ev.set("type", event.name())?;
            match &event {
                Event::Key { key } => ev.set("key", agentd_ui::normalize_key(key))?,
                Event::Change { value } => ev.set("value", lua.to_value(value)?)?,
                Event::Resize { width, height } => {
                    ev.set("width", *width)?;
                    ev.set("height", *height)?;
                }
                Event::Input { data } => ev.set("data", data.as_str())?,
                _ => {}
            }
            match node {
                None => {
                    let name = event.name().to_string();
                    let fns = with_state(lua, |s| {
                        let mut binds = Vec::new();
                        if let Event::Key { key } = &event {
                            let key = agentd_ui::normalize_key(key);
                            binds = s
                                .binds
                                .iter()
                                .filter(|(k, _)| *k == key)
                                .filter_map(|(_, f)| lua.registry_value::<Function>(f).ok())
                                .collect();
                        }
                        (binds, keys_to_fns(lua, s.chat_on.get(&(id, name))))
                    })?;
                    calls.extend(fns.0.into_iter().map(|f| (f, vec![chat_v.clone()])));
                    calls.extend(
                        fns.1
                            .into_iter()
                            .map(|f| (f, vec![Value::Table(ev.clone()), chat_v.clone()])),
                    );
                }
                Some(node) => {
                    if let Event::Resize { width, height } = &event {
                        with_state(lua, |s| {
                            if let Some(size) = s.terminal_nodes.get_mut(&(id, node)) {
                                *size = (*width, *height);
                            }
                        })?;
                    }
                    if let Event::Change { value } = &event {
                        let mut doc = chat.doc();
                        let key = match doc.kind(node) {
                            Some("checkbox") => "checked",
                            Some("list") => "selected",
                            _ => "value",
                        };
                        doc.sync(node, key, value.clone());
                    }
                    ev.set("target", el_value(lua, id, node)?)?;
                    // The nearest element, from the target up, that listens.
                    let mut at = Some(node);
                    while let Some(n) = at {
                        let fns = with_state(lua, |s| {
                            keys_to_fns(lua, s.element_on.get(&(id, n, event.name().to_string())))
                        })?;
                        if !fns.is_empty() {
                            calls.extend(
                                fns.into_iter()
                                    .map(|f| (f, vec![Value::Table(ev.clone()), chat_v.clone()])),
                            );
                            break;
                        }
                        at = chat.doc().parent(n);
                    }
                }
            }
        }
    }
    calls
        .into_iter()
        .map(|(f, args)| handler_thread(lua, f, args))
        .collect()
}

/// Run the frame functions that apply to `chat` for time `t_ms`, and
/// return what they changed.
fn render_frames(lua: &Lua, chat: &Arc<Chat>, t_ms: u64) -> Vec<Op> {
    let fns: Vec<(u64, Function)> = with_state(lua, |s| {
        s.frames
            .iter()
            .filter(|f| f.chat.is_none_or(|c| c == chat.id))
            .filter_map(|f| lua.registry_value::<Function>(&f.f).ok().map(|g| (f.id, g)))
            .collect()
    })
    .unwrap_or_default();
    if fns.is_empty() {
        return Vec::new();
    }
    lua.set_app_data(UiFrames(true));
    chat.doc().capture_begin();
    let mut failed = Vec::new();
    for (id, f) in fns {
        let outcome = lua.create_thread(f).and_then(|thread| {
            let t = t_ms as f64 / 1000.0;
            thread.resume::<MultiValue>((t, ChatUd(chat.id)))?;
            if thread.status() == ThreadStatus::Resumable {
                return Err(fail(
                    "only UI changes are allowed inside ui.frames, because frames are drawn ahead of time",
                ));
            }
            Ok(())
        });
        if let Err(e) = outcome {
            failed.push((id, e.to_string()));
        }
    }
    lua.set_app_data(UiFrames(false));
    let ops = chat.doc().capture_end();
    for (id, error) in failed {
        let _ = stop_frames(lua, id);
        report(chat, &error);
    }
    ops
}

impl agentd_ui::Script for LuaUi {
    fn opened(&self, chat: Arc<Chat>) {
        self.run(chat, Job::Opened);
    }

    fn closed(&self, id: ChatId) {
        let lua = self.lua.clone();
        self.runtime.spawn_blocking(move || {
            let lua = lua.lock().unwrap();
            let _ = with_state(&lua, |s| {
                s.chat_on.retain(|(c, _), _| *c != id);
                s.element_on.retain(|(c, _, _), _| *c != id);
                s.lists.retain(|(c, _), _| *c != id);
                s.entries.retain(|(c, _), _| *c != id);
                s.terminal_nodes.retain(|(c, _), _| *c != id);
                s.frames.retain(|f| f.chat != Some(id));
                s.frame_index
                    .lock()
                    .unwrap()
                    .retain(|(_, _, c)| *c != Some(id));
            });
        });
    }

    fn event(&self, chat: Arc<Chat>, node: Option<NodeId>, event: Event) {
        self.run(chat, Job::Event(node, event));
    }

    fn session(&self, chat: Arc<Chat>) {
        self.run(chat, Job::Session);
    }

    fn entry(&self, chat: Arc<Chat>, entry: EntryInfo) {
        self.run(chat, Job::Entry(entry));
    }

    fn animates(&self, chat: ChatId) -> Option<u32> {
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, c)| c.is_none_or(|c| c == chat))
            .map(|(_, fps, _)| *fps)
            .max()
    }

    fn render(&self, chat: &Arc<Chat>, t_ms: u64) -> Vec<Op> {
        let lua = self.lua.lock().unwrap();
        render_frames(&lua, chat, t_ms)
    }
}

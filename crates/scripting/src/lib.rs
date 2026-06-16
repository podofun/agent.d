mod channels;
mod sandbox;
mod scheduler;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use agentd_ai::{CompletionRequest, Message, Provider, Role};
use agentd_fs as fs;
use agentd_http::{Request as HttpRequest, host_of, send as http_send};
use agentd_memory::MemoryStore;
use agentd_permissions::{Permission, PermissionSet};
use agentd_runners::{RunnerDef, RunnerRegistry};
use agentd_secrets::SecretStore;
use agentd_services::{ServiceDef, ServiceRegistry};
use agentd_shell::policy::concrete_ancestor;
use agentd_shell::{ExecRequest, SandboxPolicy, exec as shell_exec};
use agentd_skills::{SkillDef, SkillRegistry};
use agentd_types::{
    ActionCall, ActionResult, CallContext, Registry, RegistryActionInfo, RegistryError,
    RegistryToolInfo,
};
use agentd_ws::{Connection as WsConnection, Frame as WsFrame, host_of as ws_host_of};
use anyhow::{Context, Result};
use async_trait::async_trait;
use mlua::{Function, Lua, LuaSerdeExt, MultiValue, RegistryKey, Table, Value};
use walkdir::WalkDir;

#[derive(Debug, Clone, Default)]
struct ActionMeta {
    tool: Option<String>,
    requires: Vec<String>,
    confirm: bool,
}

#[derive(Debug, Clone, Default)]
struct ToolMeta {
    requires: Vec<String>,
}

#[derive(Default)]
struct Catalog {
    actions: HashMap<String, RegistryKey>,
    action_meta: HashMap<String, ActionMeta>,
    tools: HashMap<String, ToolMeta>,
    /// Service-name → handler function (stored as RegistryKey so we can
    /// retrieve and call it from any thread that holds the Lua mutex).
    services: HashMap<String, RegistryKey>,
}

type SharedCatalog = Arc<RwLock<Catalog>>;

/// What context.* functions read on every call. The scheduler swaps it in
/// per coroutine resume so concurrent runners + services don't stomp each
/// other's grants.
#[derive(Debug, Clone, Default)]
pub(crate) struct ActiveContext {
    pub(crate) caller: agentd_permissions::Caller,
    pub(crate) effective_grants: PermissionSet,
    pub(crate) call_chain: Vec<String>,
    pub(crate) grant_kind: Option<String>,
    pub(crate) grant_name: Option<String>,
}

/// Where `import("name")` resolves installed packages. Set by the daemon.
#[derive(Clone, Default)]
struct PackagesRoot(Option<PathBuf>);

/// Tracks the active package-import nesting + everything each package
/// registered, so the daemon can desugar grants after load.
#[derive(Clone, Default)]
struct PackageScope(Arc<RwLock<PackageScopeInner>>);

#[derive(Default)]
struct PackageScopeInner {
    stack: Vec<String>,
    owned: HashMap<String, OwnedComponents>,
    perms: HashMap<String, Vec<String>>,
}

#[derive(Default, Clone)]
struct OwnedComponents {
    tools: Vec<String>,
    actions: Vec<String>,
    runners: Vec<String>,
    services: Vec<String>,
}

enum ComponentKind {
    Tool,
    Action,
    Runner,
    Service,
}

impl PackageScope {
    fn active(&self) -> Option<String> {
        self.0.read().unwrap().stack.last().cloned()
    }
    fn push(&self, name: &str, perms: Vec<String>) {
        let mut g = self.0.write().unwrap();
        g.stack.push(name.to_string());
        g.owned.entry(name.to_string()).or_default();
        g.perms.insert(name.to_string(), perms);
    }
    fn pop(&self) {
        self.0.write().unwrap().stack.pop();
    }
}

/// Prefix a raw registration name with the active package, if any.
/// `git` -> `acme/git`, `git.diff` -> `acme/git.diff`. Names that already
/// contain `/` (cross-package qualified) pass through untouched.
fn scoped(active: Option<&str>, raw: &str) -> String {
    match active {
        Some(pkg) if !raw.contains('/') => format!("{pkg}/{raw}"),
        _ => raw.to_string(),
    }
}

fn active_package(lua: &Lua) -> Option<String> {
    lua.app_data_ref::<PackageScope>().and_then(|s| s.active())
}

/// Scope `raw` under the active package (if any) and record it as owned by
/// that package under the given kind. Returns the (possibly prefixed) name.
fn record_and_scope(lua: &Lua, raw: &str, kind: ComponentKind) -> String {
    let scope = match lua.app_data_ref::<PackageScope>() {
        Some(s) => (*s).clone(),
        None => return raw.to_string(),
    };
    let active = scope.active();
    let name = scoped(active.as_deref(), raw);
    if let Some(pkg) = active {
        let mut g = scope.0.write().unwrap();
        let oc = g.owned.entry(pkg).or_default();
        match kind {
            ComponentKind::Tool => oc.tools.push(name.clone()),
            ComponentKind::Action => oc.actions.push(name.clone()),
            ComponentKind::Runner => oc.runners.push(name.clone()),
            ComponentKind::Service => oc.services.push(name.clone()),
        }
    }
    name
}

/// Backend handle stored in Lua app-data so `context.auth.*` can reach it.
#[derive(Clone)]
struct SecretsHolder(Option<Arc<dyn SecretStore>>);

/// Backend handle for `ctx.memory.*`.
#[derive(Clone)]
struct MemoryHolder(Option<Arc<dyn MemoryStore>>);

/// Backend handles for `context.ai.*`.
#[derive(Clone, Default)]
struct AiHolder {
    providers: HashMap<String, Arc<dyn Provider>>,
    default: Option<String>,
}

/// Root directory that `import("rel/path.lua")` and
/// `agentd.skills.load(...)` resolve against. Configured by the daemon via
/// [`LuaHost::set_root`]. Without a root set, `import` errors out.
#[derive(Default, Clone)]
struct Root(Option<PathBuf>);

/// Tracks files already imported so a second `import("foo.lua")` from
/// anywhere is a no-op. Keyed by canonicalized absolute path.
#[derive(Default, Clone)]
struct ImportCache(Arc<RwLock<HashSet<PathBuf>>>);

/// Process-wide kv backing `ctx.state`. Stored as JSON so cross-language
/// values round-trip cleanly. Locked at the map level (writes are short).
#[derive(Default, Clone)]
struct StateStore(Arc<RwLock<HashMap<String, serde_json::Value>>>);

/// Backend handle for `ctx.run(name, opts)`. The daemon wires
/// an `Arc<Executor>` (which implements `RunnerDispatcher`) via
/// [`LuaHost::set_runner_dispatcher`]. Until that's called, the Lua entry
/// point errors out with a clear message.
#[derive(Default, Clone)]
struct RunnerDispatcherHolder(Option<Arc<dyn agentd_types::RunnerDispatcher>>);

pub struct LuaHost {
    lua: Arc<Mutex<Lua>>,
    catalog: SharedCatalog,
    runners: RunnerRegistry,
    services: ServiceRegistry,
    skills: SkillRegistry,
}

impl LuaHost {
    pub fn new() -> Result<Self> {
        let lua = Lua::new();
        let catalog: SharedCatalog = Arc::new(RwLock::new(Catalog::default()));
        let runners = RunnerRegistry::new();
        let services = ServiceRegistry::new();
        let skills = SkillRegistry::new();
        lua.set_app_data(catalog.clone());
        lua.set_app_data(runners.clone());
        lua.set_app_data(services.clone());
        lua.set_app_data(skills.clone());
        lua.set_app_data(ActiveContext::default());
        lua.set_app_data(SecretsHolder(None));
        lua.set_app_data(MemoryHolder(None));
        lua.set_app_data(AiHolder::default());
        lua.set_app_data(Root::default());
        lua.set_app_data(ImportCache::default());
        lua.set_app_data(StateStore::default());
        lua.set_app_data(RunnerDispatcherHolder::default());
        lua.set_app_data(PackagesRoot::default());
        lua.set_app_data(PackageScope::default());

        install_agentd_globals(&lua, &catalog)?;
        load_helpers(&lua)?;
        sandbox::lock_down(&lua).context("lock down lua sandbox")?;

        Ok(Self {
            lua: Arc::new(Mutex::new(lua)),
            catalog,
            runners,
            services,
            skills,
        })
    }

    /// Configure the root directory that `import(...)` resolves against.
    /// Without this set, every `import` call errors out.
    pub fn set_root(&self, root: impl Into<PathBuf>) {
        let lua = self.lua.lock().unwrap();
        lua.set_app_data(Root(Some(root.into())));
    }

    /// Configure where `import("name")` resolves installed packages
    /// (`~/.local/share/agentd/packages`). Set by the daemon before init.lua.
    pub fn set_packages_root(&self, root: impl Into<PathBuf>) {
        let lua = self.lua.lock().unwrap();
        lua.set_app_data(PackagesRoot(Some(root.into())));
    }

    /// Snapshot of every package loaded this session, for grant desugaring.
    pub fn loaded_packages(&self) -> Vec<agentd_packages::LoadedPackage> {
        let lua = self.lua.lock().unwrap();
        let scope = match lua.app_data_ref::<PackageScope>() {
            Some(s) => (*s).clone(),
            None => return Vec::new(),
        };
        let g = scope.0.read().unwrap();
        g.owned
            .iter()
            .map(|(name, oc)| agentd_packages::LoadedPackage {
                name: name.clone(),
                permissions: g.perms.get(name).cloned().unwrap_or_default(),
                tools: oc.tools.clone(),
                actions: oc.actions.clone(),
                runners: oc.runners.clone(),
                services: oc.services.clone(),
            })
            .collect()
    }

    /// Shared handle to the runner registry populated by `agentd.runner{...}`.
    pub fn runners(&self) -> RunnerRegistry {
        self.runners.clone()
    }

    /// Shared handle to the service registry populated by `agentd.service(name, fn)`.
    pub fn services(&self) -> ServiceRegistry {
        self.services.clone()
    }

    /// Kick off the background runtime that drives `async(fn)` callbacks.
    /// Must be called by the daemon once a Tokio runtime is current
    /// (typically right after `LuaHost::new`). Until this is invoked,
    /// `async(...)` returns an error.
    pub fn start_async_runtime(&self, handle: tokio::runtime::Handle) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<scheduler::AsyncTask>();
        let lua_handle = self.lua.clone();
        handle.spawn(async move {
            while let Some(task) = rx.recv().await {
                let lua = lua_handle.clone();
                tokio::spawn(async move {
                    let result = scheduler::drive(lua, task.thread, vec![], task.ctx).await;
                    task.state.set(match result {
                        Ok(v) => Ok(v),
                        Err(e) => Err(e.to_string()),
                    });
                });
            }
        });
        let lua = self.lua.lock().unwrap();
        lua.set_app_data(scheduler::AsyncTaskSpawner(tx));
    }

    /// Shared handle to the skill registry populated by `agentd.skill{...}`
    /// and `agentd.skills.load{,_dir}(...)`.
    pub fn skills(&self) -> SkillRegistry {
        self.skills.clone()
    }

    /// Run a single Lua source file. Used by the daemon to evaluate the
    /// configured `init.lua` entry point.
    pub fn load_file(&self, path: &Path) -> Result<()> {
        let src =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let lua = self.lua.lock().unwrap();
        lua.load(&src)
            .set_name(path.to_string_lossy())
            .exec()
            .with_context(|| format!("exec {}", path.display()))?;
        // Mark this file as already imported so a later `import(...)`
        // referencing the same canonical path becomes a no-op.
        if let Ok(canon) = path.canonicalize()
            && let Some(cache) = lua.app_data_ref::<ImportCache>()
        {
            cache.0.write().unwrap().insert(canon);
        }
        Ok(())
    }

    /// Wire a secret store backend. Call before `load_dir` so tool code can
    /// rely on it. `MemoryStore` for tests, `KeyringStore` in production.
    pub fn set_secrets(&self, store: Arc<dyn SecretStore>) {
        let lua = self.lua.lock().unwrap();
        lua.set_app_data(SecretsHolder(Some(store)));
    }

    /// Wire the memory store backend. `RedbStore` in production,
    /// `MemMemoryStore` in tests. Call before `load_dir`/`load_file`.
    pub fn set_memory(&self, store: Arc<dyn MemoryStore>) {
        let lua = self.lua.lock().unwrap();
        lua.set_app_data(MemoryHolder(Some(store)));
    }

    /// Register a named AI provider.
    pub fn set_ai_provider(&self, name: impl Into<String>, provider: Arc<dyn Provider>) {
        let lua = self.lua.lock().unwrap();
        let mut holder: AiHolder = lua
            .app_data_ref::<AiHolder>()
            .map(|h| (*h).clone())
            .unwrap_or_default();
        holder.providers.insert(name.into(), provider);
        lua.set_app_data(holder);
    }

    /// Pick the default provider name returned when Lua calls
    /// `context.ai.ask` without a `provider` opt.
    pub fn set_default_ai_provider(&self, name: impl Into<String>) {
        let lua = self.lua.lock().unwrap();
        let mut holder: AiHolder = lua
            .app_data_ref::<AiHolder>()
            .map(|h| (*h).clone())
            .unwrap_or_default();
        holder.default = Some(name.into());
        lua.set_app_data(holder);
    }

    /// Wire the dispatcher backing `ctx.run(name, opts)`. The
    /// daemon passes its `Arc<Executor>` here; tests can pass a mock
    /// implementation of `RunnerDispatcher`. Must be called before `init.lua`
    /// invokes `ctx.run` — calling it later is also fine, but
    /// scripts that touched the API earlier will have errored out.
    pub fn set_runner_dispatcher(&self, dispatcher: Arc<dyn agentd_types::RunnerDispatcher>) {
        let lua = self.lua.lock().unwrap();
        lua.set_app_data(RunnerDispatcherHolder(Some(dispatcher)));
    }

    pub fn load_dir(&self, dir: &Path) -> Result<usize> {
        if !dir.exists() {
            tracing::warn!(path = %dir.display(), "tools dir missing");
            return Ok(0);
        }
        let lua = self.lua.lock().unwrap();
        let mut count = 0;
        for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("lua") {
                continue;
            }
            let src = std::fs::read_to_string(path)
                .with_context(|| format!("read {}", path.display()))?;
            lua.load(&src)
                .set_name(path.to_string_lossy())
                .exec()
                .with_context(|| format!("exec {}", path.display()))?;
            count += 1;
        }
        Ok(count)
    }
}

/// Wrap a Rust C-function so that it can yield. The C function returns either
/// an `OpMarker` userdata (when called inside a coroutine) or its final value
/// (when called from the top level — block_on path). The wrapper is a Lua
/// closure that calls the C fn and, if the return is a marker, invokes
/// `coroutine.yield` from the Lua frame — sidestepping the
/// "attempt to yield across a C-call boundary" restriction in Lua 5.4.
fn yieldable_wrap(lua: &Lua, internal: Function) -> mlua::Result<Function> {
    let chunk_src = r#"
        local internal = ...
        return function(...)
          local v = internal(...)
          if type(v) == "userdata" then
            local r = coroutine.yield(v)
            if type(r) == "table" and r.ok == false then
              error(r.error or "scheduler error", 0)
            end
            return r
          end
          return v
        end
    "#;
    lua.load(chunk_src)
        .set_name("yieldable_wrap")
        .call::<Function>(internal)
}

fn install_agentd_globals(lua: &Lua, _catalog: &SharedCatalog) -> Result<()> {
    let agentd = lua.create_table()?;
    // Define-one verbs (load-time): one noun each — tool/action/runner/skill/service.
    agentd.set("action", lua.create_function(register_action_dispatch)?)?;
    agentd.set("tool", lua.create_function(register_tool)?)?;
    agentd.set("runner", lua.create_function(register_runner)?)?;
    agentd.set("skill", lua.create_function(register_skill)?)?;
    agentd.set("service", lua.create_function(register_service)?)?;
    agentd.set("skills", build_skills_table(lua)?)?;
    // Manage-collection tables (load-time): plural noun + `.list`.
    agentd.set("tools", build_collection_table(lua, ListKind::Tools)?)?;
    agentd.set("actions", build_collection_table(lua, ListKind::Actions)?)?;
    agentd.set("runners", build_collection_table(lua, ListKind::Runners)?)?;
    agentd.set("services", build_collection_table(lua, ListKind::Services)?)?;

    lua.globals().set("agentd", agentd)?;

    // Bare globals — builtins. `import` joins the control-flow primitives.
    lua.globals()
        .set("import", lua.create_function(agentd_import)?)?;
    install_json_global(lua)?;
    channels::install_channel_global(lua)?;
    // `sleep(ms)` — yieldable timer. Outside a coroutine, falls back to
    // blocking the current OS thread (rare; usually only init.lua).
    let sleep_internal = lua.create_function(sleep_binding)?;
    lua.globals()
        .set("sleep", yieldable_wrap(lua, sleep_internal)?)?;
    lua.globals()
        .set("async", lua.create_function(async_spawn)?)?;
    // `await` is yield-aware: when called inside a coroutine the C fn
    // returns an OpMarker, and the Lua wrapper performs the actual yield.
    let await_internal_fn = lua.create_function(await_internal)?;
    lua.globals()
        .set("await", yieldable_wrap(lua, await_internal_fn)?)?;

    // `ctx` capability handle — built once, stored under app-data, and exposed
    // as the temporary global `__agentd_ctx` so helpers.lua can augment
    // `ctx.http` / `ctx.ws`. Lockdown nils the global; the table survives via
    // the stored RegistryKey and the handler-injection shim.
    build_and_store_ctx(lua)?;
    Ok(())
}

/// Which registry a `agentd.<collection>.list()` reads.
enum ListKind {
    Tools,
    Actions,
    Runners,
    Services,
}

fn names_to_table(lua: &Lua, mut names: Vec<String>) -> mlua::Result<Table> {
    names.sort();
    let t = lua.create_table()?;
    for (i, n) in names.into_iter().enumerate() {
        t.set(i + 1, n)?;
    }
    Ok(t)
}

/// Build an `agentd.<noun>s` collection table exposing a single `.list()`.
fn build_collection_table(lua: &Lua, kind: ListKind) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let f = match kind {
        ListKind::Tools => lua.create_function(|lua, _: MultiValue| {
            let cat = lua
                .app_data_ref::<SharedCatalog>()
                .ok_or_else(|| mlua::Error::external("scripting catalog missing"))?;
            let names = cat.read().unwrap().tools.keys().cloned().collect();
            names_to_table(lua, names)
        })?,
        ListKind::Actions => lua.create_function(|lua, _: MultiValue| {
            let cat = lua
                .app_data_ref::<SharedCatalog>()
                .ok_or_else(|| mlua::Error::external("scripting catalog missing"))?;
            let names = cat.read().unwrap().actions.keys().cloned().collect();
            names_to_table(lua, names)
        })?,
        ListKind::Runners => lua.create_function(|lua, _: MultiValue| {
            let reg = lua
                .app_data_ref::<RunnerRegistry>()
                .ok_or_else(|| mlua::Error::external("runner registry missing"))?;
            names_to_table(lua, reg.names())
        })?,
        ListKind::Services => lua.create_function(|lua, _: MultiValue| {
            let reg = lua
                .app_data_ref::<ServiceRegistry>()
                .ok_or_else(|| mlua::Error::external("service registry missing"))?;
            names_to_table(lua, reg.names())
        })?,
    };
    t.set("list", f)?;
    Ok(t)
}

/// Registry key under which the single `ctx` facade table is stored.
struct CtxTable(RegistryKey);

/// Build the per-invocation `ctx` capability table, store it under app-data,
/// and expose it as the temporary global `__agentd_ctx`. Its methods read the
/// live `ActiveContext` from app-data, which the scheduler swaps per resume —
/// so the single shared table stays correct for every concurrent invocation.
fn build_and_store_ctx(lua: &Lua) -> mlua::Result<()> {
    let ctx = lua.create_table()?;
    ctx.set("log", build_log_table(lua)?)?;
    // `ctx.shell(bin, args?, opts?)` — single op, so the table is the callable.
    let shell_internal = lua.create_function(shell_exec_binding)?;
    ctx.set("shell", yieldable_wrap(lua, shell_internal)?)?;
    ctx.set("fs", build_fs_table(lua)?)?;
    ctx.set("http", build_http_table(lua)?)?;
    ctx.set("secret", build_secret_table(lua)?)?;
    ctx.set("ai", build_ai_table(lua)?)?;
    ctx.set("ws", build_ws_table(lua)?)?;
    ctx.set("state", build_state_table(lua)?)?;
    ctx.set("memory", build_memory_table(lua)?)?;
    ctx.set("caller", build_caller_table(lua)?)?;
    ctx.set("tools", lua.create_function(tools_list_binding)?)?;

    // `ctx.run(name, prompt_or_opts)` — coercion wrapper over the yieldable
    // runner dispatch binding (reuse the same wrap, no double-wrap).
    let run_wrapped = yieldable_wrap(lua, lua.create_function(runner_dispatch_binding)?)?;
    let run_fn: Function = lua
        .load(
            r#"
        local dispatch = ...
        return function(name, opts)
          if type(opts) == "string" then opts = { prompt = opts } end
          return dispatch(name, opts or {})
        end
    "#,
        )
        .set_name("ctx.run")
        .call(run_wrapped)?;
    ctx.set("run", run_fn)?;

    // `ctx.call(name, args?)` — recursive dispatch. Threads `ctx` into the
    // inner handler so a tool-called action's `function(args, ctx)` works.
    let resolve = lua.create_function(tools_resolve_binding)?;
    let pop_chain = lua.create_function(tools_pop_chain_binding)?;
    let call_fn: Function = lua
        .load(
            r#"
        local resolve, pop_chain, ctx = ...
        return function(name, args)
          local handler = resolve(name, args)
          local ok, result = pcall(handler, args, ctx)
          pop_chain()
          if not ok then error(result, 0) end
          return result
        end
    "#,
        )
        .set_name("ctx.call")
        .call((resolve, pop_chain, ctx.clone()))?;
    ctx.set("call", call_fn)?;

    let key = lua.create_registry_value(ctx.clone())?;
    lua.set_app_data(CtxTable(key));
    // `ctx` is NOT a permanent global — it is injected as the handler/service
    // parameter by `ctx_thread`. The temporary `__agentd_ctx` global lets
    // helpers.lua augment `ctx.http` / `ctx.ws` at load time; sandbox lockdown
    // nils it afterward (not on the allow-list), but the table survives via the
    // stored RegistryKey and the helpers' captured upvalue.
    lua.globals().set("__agentd_ctx", ctx)?;
    Ok(())
}

/// Wrap a user handler/service `func` in a Lua shim that injects the shared
/// `ctx` table, then return a coroutine over the shim. `with_args = true` for
/// action handlers (`function(args) return h(args, ctx) end`); `false` for
/// service bodies (`function() return body(ctx) end`).
fn ctx_thread(lua: &Lua, func: Function, with_args: bool) -> mlua::Result<mlua::Thread> {
    let ctx_tbl: Table = {
        let ctx_key = lua
            .app_data_ref::<CtxTable>()
            .ok_or_else(|| mlua::Error::external("ctx table missing"))?;
        lua.registry_value(&ctx_key.0)?
    };
    let shim_src = if with_args {
        "local h, ctx = ...\nreturn function(args) return h(args, ctx) end"
    } else {
        "local body, ctx = ...\nreturn function() return body(ctx) end"
    };
    let shim: Function = lua
        .load(shim_src)
        .set_name("ctx shim")
        .call((func, ctx_tbl))?;
    lua.create_thread(shim)
}

// ---------- register / tool ----------

fn register_action_dispatch(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    if args.len() == 2 {
        let mut iter = args.into_iter();
        let name: String = lua.unpack(iter.next().unwrap())?;
        let func: Function = lua.unpack(iter.next().unwrap())?;
        return register_named(lua, name, func, ActionMeta::default());
    }
    if args.len() == 1 {
        let v = args.into_iter().next().unwrap();
        return register_action(lua, v);
    }
    Err(mlua::Error::external(format!(
        "agentd.action: expected 1 or 2 args, got {}",
        args.len()
    )))
}

fn register_action(lua: &Lua, args: Value) -> mlua::Result<()> {
    let (name, func, meta) = match args {
        Value::String(s) => {
            return Err(mlua::Error::external(format!(
                "agentd.action('{}'): handler required as second argument",
                s.to_string_lossy()
            )));
        }
        Value::Table(t) => parse_register_table(t)?,
        other => {
            return Err(mlua::Error::external(format!(
                "agentd.action: expected (name, fn) or table, got {}",
                other.type_name()
            )));
        }
    };
    register_named(lua, name, func, meta)
}

fn parse_register_table(t: Table) -> mlua::Result<(String, Function, ActionMeta)> {
    let name: String = t
        .get("name")
        .map_err(|_| mlua::Error::external("agentd.action{...}: `name` is required"))?;
    let func: Function = t.get("handler").map_err(|_| {
        mlua::Error::external("agentd.action{...}: `handler` (function) is required")
    })?;
    let requires = read_string_array(&t, "requires")?;
    let confirm: bool = t.get::<Option<bool>>("confirm")?.unwrap_or(false);
    let tool: Option<String> = t.get::<Option<String>>("tool")?;
    let inferred_tool = tool.or_else(|| name.split_once('.').map(|(t, _)| t.to_string()));
    Ok((
        name,
        func,
        ActionMeta {
            tool: inferred_tool,
            requires,
            confirm,
        },
    ))
}

fn register_named(
    lua: &Lua,
    name: String,
    func: Function,
    mut meta: ActionMeta,
) -> mlua::Result<()> {
    // Prefix + record under the active package, if any.
    let name = record_and_scope(lua, &name, ComponentKind::Action);
    // Inside a package, an explicit/inferred unqualified tool name is prefixed
    // too, so the action's tool matches the registered `pkg/tool`.
    if let Some(pkg) = active_package(lua) {
        meta.tool = match meta.tool.take() {
            Some(t) if !t.contains('/') => Some(format!("{pkg}/{t}")),
            other => other,
        };
    }
    if meta.tool.is_none() {
        meta.tool = name.split_once('.').map(|(t, _)| t.to_string());
    }
    let key = lua.create_registry_value(func)?;
    let catalog = lua
        .app_data_ref::<SharedCatalog>()
        .ok_or_else(|| mlua::Error::external("scripting catalog missing"))?;
    let mut guard = catalog
        .write()
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    if let Some(old) = guard.actions.insert(name.clone(), key) {
        let _ = lua.remove_registry_value(old);
    }
    guard.action_meta.insert(name.clone(), meta);
    tracing::info!(action = %name, "registered action");
    Ok(())
}

fn register_tool(lua: &Lua, t: Table) -> mlua::Result<()> {
    let name: String = t
        .get("name")
        .map_err(|_| mlua::Error::external("agentd.tool{...}: `name` is required"))?;
    let name = record_and_scope(lua, &name, ComponentKind::Tool);
    let requires = read_string_array(&t, "requires")?;
    let catalog = lua
        .app_data_ref::<SharedCatalog>()
        .ok_or_else(|| mlua::Error::external("scripting catalog missing"))?;
    let mut guard = catalog
        .write()
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    guard.tools.insert(name.clone(), ToolMeta { requires });
    tracing::info!(tool = %name, "registered tool");
    Ok(())
}

// ---------- agentd.runner ----------

fn register_runner(lua: &Lua, t: Table) -> mlua::Result<()> {
    let name: String = t
        .get("name")
        .map_err(|_| mlua::Error::external("agentd.runner{...}: `name` is required"))?;
    let name = record_and_scope(lua, &name, ComponentKind::Runner);
    let system: Option<String> = t.get::<Option<String>>("system")?;
    // Model carries provider as a prefix: "anthropic/claude-opus-4-7". The
    // legacy `provider` + bare `model` shape is rejected so we stop having
    // to keep both alive in parallel.
    if t.get::<Option<String>>("provider")?.is_some() {
        return Err(mlua::Error::external(
            "agentd.runner{...}: `provider` field is gone — use `model = \"<provider>/<model>\"` instead",
        ));
    }
    let model: Option<String> = t.get::<Option<String>>("model")?;
    let skills = read_string_array(&t, "skills")?;
    let mut allowed_actions = read_string_array(&t, "allowed_actions")?;
    if allowed_actions.is_empty() {
        allowed_actions = read_string_array(&t, "actions")?;
    }
    // Intra-package: rewrite unqualified action names to `pkg/...` so the
    // runner's allowlist matches the package's prefixed actions.
    if let Some(pkg) = active_package(lua) {
        allowed_actions = allowed_actions
            .into_iter()
            .map(|a| scoped(Some(pkg.as_str()), &a))
            .collect();
    }

    let def = RunnerDef {
        name: name.clone(),
        system,
        model,
        skills,
        allowed_actions,
    };
    let runners = lua
        .app_data_ref::<RunnerRegistry>()
        .ok_or_else(|| mlua::Error::external("runner registry missing"))?;
    runners.insert(def);
    tracing::info!(runner = %name, "registered runner");
    Ok(())
}

// ---------- sleep (bare global, yieldable) ----------

fn sleep_binding(lua: &Lua, ms: u64) -> mlua::Result<Value> {
    let d = std::time::Duration::from_millis(ms);
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::Sleep(d));
    }
    // Top-level fallback: no scheduler to yield to, block the OS thread.
    std::thread::sleep(d);
    Ok(Value::Nil)
}

// ---------- service / async / await (bare globals) ----------

fn register_service(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    // Accepted shapes:
    //   agentd.service("name", fn)
    //   agentd.service("name", { restart = "always", backoff_ms = 5000, ... }, fn)
    let mut iter = args.into_iter();
    let name: String = match iter.next() {
        Some(v) => lua.unpack(v)?,
        None => return Err(mlua::Error::external("service: name is required")),
    };
    let name = record_and_scope(lua, &name, ComponentKind::Service);
    let mut opts_table: Option<Table> = None;
    let mut func: Option<Function> = None;
    if let Some(v) = iter.next() {
        match v {
            Value::Function(f) => func = Some(f),
            Value::Table(t) => {
                opts_table = Some(t);
                func = match iter.next() {
                    Some(Value::Function(f)) => Some(f),
                    Some(other) => {
                        return Err(mlua::Error::external(format!(
                            "service `{name}`: third arg must be a function, got {}",
                            other.type_name()
                        )));
                    }
                    None => None,
                };
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "service `{name}`: second arg must be a function or opts table, got {}",
                    other.type_name()
                )));
            }
        }
    }
    let func = func.ok_or_else(|| {
        mlua::Error::external(format!("service `{name}`: handler function is required"))
    })?;
    let mut def = ServiceDef {
        name: name.clone(),
        tool: None,
        source: None,
        restart: None,
        backoff_ms: None,
        backoff_max_ms: None,
    };
    if let Some(t) = opts_table {
        def.restart = t.get::<Option<String>>("restart")?;
        def.backoff_ms = t.get::<Option<u64>>("backoff_ms")?;
        def.backoff_max_ms = t.get::<Option<u64>>("backoff_max_ms")?;
        // Light validation — unknown restart strings are normalised to `None`
        // (== "never") with a warn so typos surface.
        if let Some(r) = def.restart.as_deref()
            && !matches!(r, "always" | "on_failure" | "never")
        {
            tracing::warn!(service = %name, restart = %r,
                    "unknown restart policy; treating as `never`");
            def.restart = None;
        }
    }
    let key = lua.create_registry_value(func)?;
    {
        let catalog = lua
            .app_data_ref::<SharedCatalog>()
            .ok_or_else(|| mlua::Error::external("scripting catalog missing"))?;
        let mut guard = catalog
            .write()
            .map_err(|e| mlua::Error::external(e.to_string()))?;
        if let Some(old) = guard.services.insert(name.clone(), key) {
            let _ = lua.remove_registry_value(old);
        }
    }
    let services = lua
        .app_data_ref::<ServiceRegistry>()
        .ok_or_else(|| mlua::Error::external("service registry missing"))?;
    services.insert(def);
    tracing::info!(service = %name, "registered service");
    Ok(())
}

/// `async(fn)` — schedule `fn` to run as a background coroutine on its own
/// Tokio task. Returns a handle that `await(...)` resolves later. Multiple
/// async tasks make progress in parallel because IO yields the Lua mutex —
/// CPU-bound Lua chunks still serialize on the one Lua state, JS-style.
fn async_spawn(lua: &Lua, func: Function) -> mlua::Result<scheduler::AsyncHandle> {
    let thread = lua.create_thread(func)?;
    let state = scheduler::AsyncHandleState::new();
    let spawner = lua
        .app_data_ref::<scheduler::AsyncTaskSpawner>()
        .ok_or_else(|| {
            mlua::Error::external(
                "async: background runtime not started (daemon must call \
                 LuaHost::start_async_runtime before init.lua)",
            )
        })?;
    // Inherit caller's ActiveContext so the async body sees the same
    // effective_grants. Snapshot now because the parent coroutine's
    // context will be re-set on its next resume, not ours.
    let inherited = lua
        .app_data_ref::<ActiveContext>()
        .map(|a| a.clone())
        .unwrap_or_default();
    spawner
        .0
        .send(scheduler::AsyncTask {
            thread,
            state: state.clone(),
            ctx: inherited,
        })
        .map_err(|e| mlua::Error::external(format!("async: send: {e}")))?;
    Ok(scheduler::AsyncHandle(state))
}

/// `await(handle)` — block the current coroutine until the async handle's
/// task completes and return its value. Yields to the scheduler so peer
/// coroutines / async tasks keep making progress. Outside a coroutine
/// (e.g. the top of init.lua), falls back to a thread-blocking wait.
fn await_internal(lua: &Lua, handle: mlua::AnyUserData) -> mlua::Result<Value> {
    let state = {
        let h = handle.borrow::<scheduler::AsyncHandle>()?;
        h.0.clone()
    };
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::Await(state));
    }
    // Non-coroutine fallback: block. Rare — usually only init.lua does this.
    let handle_rt = tokio::runtime::Handle::try_current()
        .map_err(|e| mlua::Error::external(format!("await: no tokio runtime: {e}")))?;
    let result = handle_rt.block_on(async move {
        loop {
            if let Some(v) = state.read() {
                return v;
            }
            state.notify.notified().await;
        }
    });
    match result {
        Ok(v) => Ok(lua.to_value(&v)?),
        Err(msg) => Err(mlua::Error::external(msg)),
    }
}

// ---------- agentd.skill ----------

fn register_skill(lua: &Lua, t: Table) -> mlua::Result<()> {
    let name: String = t
        .get("name")
        .map_err(|_| mlua::Error::external("agentd.skill{...}: `name` is required"))?;
    let description: Option<String> = t.get::<Option<String>>("description")?;
    let system: String = t.get::<Option<String>>("system")?.unwrap_or_default();
    let actions = read_string_array(&t, "actions")?;

    let def = SkillDef {
        name: name.clone(),
        description,
        system: system.trim().to_string(),
        actions,
        source: None,
    };
    let skills = lua
        .app_data_ref::<SkillRegistry>()
        .ok_or_else(|| mlua::Error::external("skill registry missing"))?;
    skills.insert(def);
    tracing::info!(skill = %name, "registered skill");
    Ok(())
}

// ---------- agentd.skills.* (Markdown loader) ----------

fn build_skills_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("load", lua.create_function(skills_load_binding)?)?;
    t.set("dir", lua.create_function(skills_load_dir_binding)?)?;
    t.set("list", lua.create_function(skills_list_binding)?)?;
    Ok(t)
}

fn skills_load_binding(lua: &Lua, rel: String) -> mlua::Result<String> {
    let resolved = resolve_import_path(lua, &rel)?;
    let skills = lua
        .app_data_ref::<SkillRegistry>()
        .ok_or_else(|| mlua::Error::external("skill registry missing"))?;
    let def = skills
        .load_file(&resolved)
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    tracing::info!(skill = %def.name, path = %resolved.display(), "loaded skill file");
    Ok(def.name)
}

fn skills_load_dir_binding(lua: &Lua, rel: String) -> mlua::Result<usize> {
    let resolved = resolve_import_path(lua, &rel)?;
    let skills = lua
        .app_data_ref::<SkillRegistry>()
        .ok_or_else(|| mlua::Error::external("skill registry missing"))?;
    let n = skills
        .load_dir(&resolved)
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    tracing::info!(loaded = n, dir = %resolved.display(), "loaded skills dir");
    Ok(n)
}

fn skills_list_binding(lua: &Lua, _: MultiValue) -> mlua::Result<Table> {
    let skills = lua
        .app_data_ref::<SkillRegistry>()
        .ok_or_else(|| mlua::Error::external("skill registry missing"))?;
    let names = skills.names();
    let t = lua.create_table()?;
    for (i, n) in names.into_iter().enumerate() {
        t.set(i + 1, n)?;
    }
    Ok(t)
}

// ---------- import ----------
//
// Sandboxed file loader. Resolves a path relative to the configured root,
// rejects absolute paths and `..` traversal, dedupes by canonical path so
// repeated imports are no-ops. Returns the value the chunk produced (or
// `true` if the chunk returned nothing) — same semantics as Lua `require`.

fn agentd_import(lua: &Lua, rel: String) -> mlua::Result<Value> {
    // Bare identifier (no `/`, no `.lua`) => installed package.
    if !rel.contains('/') && !rel.ends_with(".lua") {
        return import_package(lua, &rel);
    }
    let resolved = resolve_import_path(lua, &rel)?;
    // Canonicalize for the cache key. If it fails (file missing, broken
    // symlink), fall back to the resolved path so we still surface a useful
    // read error below.
    let canon = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());

    {
        let cache = lua
            .app_data_ref::<ImportCache>()
            .ok_or_else(|| mlua::Error::external("import cache missing"))?;
        if cache.0.read().unwrap().contains(&canon) {
            return Ok(Value::Boolean(true));
        }
    }

    let src = std::fs::read_to_string(&resolved)
        .map_err(|e| mlua::Error::external(format!("import `{rel}`: {e}")))?;

    // Mark loaded BEFORE exec so cycles don't loop forever. Same trick Lua's
    // own `require` uses.
    {
        let cache = lua
            .app_data_ref::<ImportCache>()
            .ok_or_else(|| mlua::Error::external("import cache missing"))?;
        cache.0.write().unwrap().insert(canon.clone());
    }

    let chunk = lua.load(&src).set_name(resolved.to_string_lossy());
    let ret: Value = chunk.eval()?;
    Ok(match ret {
        Value::Nil => Value::Boolean(true),
        other => other,
    })
}

/// Load an installed package by name from the configured packages root.
/// Pushes a package scope so registrations during the package's entry (and any
/// relative files it imports) are prefixed + owner-tagged, then pops it.
fn import_package(lua: &Lua, name: &str) -> mlua::Result<Value> {
    let root = lua
        .app_data_ref::<PackagesRoot>()
        .and_then(|r| r.0.clone())
        .ok_or_else(|| {
            mlua::Error::external(
                "import: packages root not configured (daemon must call set_packages_root)",
            )
        })?;
    let dir = root.join(name);
    let manifest = agentd_packages::Manifest::load(&dir.join("package.toml"))
        .map_err(|e| mlua::Error::external(format!("package `{name}`: {e}")))?;

    let scope = lua
        .app_data_ref::<PackageScope>()
        .map(|s| (*s).clone())
        .ok_or_else(|| mlua::Error::external("package scope missing"))?;
    scope.push(&manifest.name, manifest.permissions.clone());

    let entry = dir.join(&manifest.entry);
    let result = std::fs::read_to_string(&entry)
        .map_err(|e| mlua::Error::external(format!("package `{name}` entry: {e}")))
        .and_then(|src| lua.load(&src).set_name(entry.to_string_lossy()).exec());
    scope.pop();
    result.map(|_| Value::Boolean(true))
}

fn resolve_import_path(lua: &Lua, rel: &str) -> mlua::Result<PathBuf> {
    if rel.is_empty() {
        return Err(mlua::Error::external("import: path is empty"));
    }
    let root = {
        let r = lua
            .app_data_ref::<Root>()
            .ok_or_else(|| mlua::Error::external("root holder missing"))?;
        r.0.clone().ok_or_else(|| {
            mlua::Error::external("import: no root configured (daemon must call set_root)")
        })?
    };
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(mlua::Error::external(format!(
            "import `{rel}`: absolute paths are not allowed"
        )));
    }
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                return Err(mlua::Error::external(format!(
                    "import `{rel}`: `..` traversal is not allowed"
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(mlua::Error::external(format!(
                    "import `{rel}`: prefix/root components are not allowed"
                )));
            }
            _ => {}
        }
    }
    Ok(root.join(p))
}

fn read_string_array(t: &Table, key: &str) -> mlua::Result<Vec<String>> {
    match t.get::<Value>(key) {
        Ok(Value::Nil) => Ok(Vec::new()),
        Ok(Value::Table(arr)) => arr
            .sequence_values::<String>()
            .collect::<mlua::Result<Vec<_>>>(),
        Ok(other) => Err(mlua::Error::external(format!(
            "`{key}` must be an array of strings, got {}",
            other.type_name()
        ))),
        Err(_) => Ok(Vec::new()),
    }
}

// ---------- ctx.state (process-wide kv) ----------
//
// Tiny shared map for cross-coroutine + cross-service state. Stored as
// `serde_json::Value` so the value survives a round-trip through the
// Lua/Tokio boundary unchanged. Keys are strings. Nothing here is
// permission-gated; treat as in-process scratch space.

fn build_state_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("get", lua.create_function(state_get_binding)?)?;
    t.set("set", lua.create_function(state_set_binding)?)?;
    t.set("delete", lua.create_function(state_delete_binding)?)?;
    t.set("keys", lua.create_function(state_keys_binding)?)?;
    t.set("clear", lua.create_function(state_clear_binding)?)?;
    Ok(t)
}

fn state_get_binding(lua: &Lua, key: String) -> mlua::Result<Value> {
    let store = lua
        .app_data_ref::<StateStore>()
        .ok_or_else(|| mlua::Error::external("state: store missing"))?;
    let snapshot = store.0.read().unwrap().get(&key).cloned();
    match snapshot {
        Some(v) => lua.to_value(&v),
        None => Ok(Value::Nil),
    }
}

fn state_set_binding(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    let mut it = args.into_iter();
    let key: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("state.set: key required"))?,
    )?;
    let v = it
        .next()
        .ok_or_else(|| mlua::Error::external("state.set: value required"))?;
    let json: serde_json::Value = lua
        .from_value(v)
        .map_err(|e| mlua::Error::external(format!("state.set: serialize: {e}")))?;
    let store = lua
        .app_data_ref::<StateStore>()
        .ok_or_else(|| mlua::Error::external("state: store missing"))?;
    store.0.write().unwrap().insert(key, json);
    Ok(())
}

fn state_delete_binding(lua: &Lua, key: String) -> mlua::Result<bool> {
    let store = lua
        .app_data_ref::<StateStore>()
        .ok_or_else(|| mlua::Error::external("state: store missing"))?;
    Ok(store.0.write().unwrap().remove(&key).is_some())
}

fn state_keys_binding(lua: &Lua, _: MultiValue) -> mlua::Result<Table> {
    let store = lua
        .app_data_ref::<StateStore>()
        .ok_or_else(|| mlua::Error::external("state: store missing"))?;
    let g = store.0.read().unwrap();
    let mut keys: Vec<String> = g.keys().cloned().collect();
    keys.sort();
    let t = lua.create_table()?;
    for (i, k) in keys.into_iter().enumerate() {
        t.set(i + 1, k)?;
    }
    Ok(t)
}

fn state_clear_binding(lua: &Lua, _: MultiValue) -> mlua::Result<()> {
    let store = lua
        .app_data_ref::<StateStore>()
        .ok_or_else(|| mlua::Error::external("state: store missing"))?;
    store.0.write().unwrap().clear();
    Ok(())
}

// ---------- ctx.run dispatcher binding ----------
//
// The Lua surface is installed in `helpers.lua` as `ctx.run`,
// which forwards into `agentd._runner_dispatch(name, opts)`. This C binding
// is yieldable: it builds a `RunnerDispatch` op and parks the calling
// coroutine while the executor drives the underlying runner.

fn runner_dispatch_binding(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut it = args.into_iter();
    let name: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("runners.run: name required"))?,
    )?;
    let opts_value = it.next().unwrap_or(Value::Nil);
    let opts_json: serde_json::Value = match opts_value {
        Value::Nil => serde_json::Value::Object(Default::default()),
        v => lua
            .from_value(v)
            .map_err(|e| mlua::Error::external(format!("runners.run: opts: {e}")))?,
    };
    let dispatcher = {
        let h = lua
            .app_data_ref::<RunnerDispatcherHolder>()
            .ok_or_else(|| mlua::Error::external("runner dispatcher: holder missing"))?;
        h.0.clone().ok_or_else(|| {
            mlua::Error::external(
                "runners.run: no dispatcher wired \
                 (daemon must call LuaHost::set_runner_dispatcher)",
            )
        })?
    };
    // Caller identity inherits from the calling coroutine via ActiveContext —
    // the real caller (interface/session/user) is carried verbatim, so the
    // engine's per-interface allowlist and any downstream `ctx.caller` reads
    // see the original identity. The executor overrides `runner` when it
    // composes the underlying call. Only when no interface is present (e.g. a
    // bare service context) do we synthesise one from the call chain's
    // outermost entry so layer-4 still has something to gate on.
    let caller = {
        let active = lua
            .app_data_ref::<ActiveContext>()
            .ok_or_else(|| mlua::Error::external("active context missing"))?;
        let mut c = active.caller.clone();
        if c.interface.is_none() && c.service.is_none() {
            c.interface = Some(
                active
                    .call_chain
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "lua".into())
                    .into(),
            );
        }
        c
    };
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(
            lua,
            scheduler::Op::RunnerRun {
                dispatcher,
                caller,
                name,
                opts: opts_json,
            },
        );
    }
    // Top-level fallback: block.
    let result = block_on(dispatcher.run_runner_json(caller, &name, opts_json))?;
    match result {
        Ok(v) => lua.to_value(&v),
        Err(e) => Err(mlua::Error::external(e)),
    }
}

// ---------- helpers.lua loader ----------
//
// Pure-Lua surface (try, timer, http.client, ws extras, runners wrapper).
// Embedded via `include_str!` so the binary stays self-contained.

const HELPERS_SRC: &str = include_str!("helpers.lua");

fn load_helpers(lua: &Lua) -> Result<()> {
    lua.load(HELPERS_SRC)
        .set_name("helpers.lua")
        .exec()
        .context("load helpers.lua")?;
    Ok(())
}

// ---------- json (bare global + agentd.json alias) ----------
//
// `json.null` is a unique sentinel table. `json.encode` maps it back to a
// real JSON `null`; everywhere else it stays out of the way (no light-
// userdata leaking up into user code). `json.decode` replaces JSON nulls
// with this sentinel so user code can compare against `json.null`. To get
// the historical "nil for null" behaviour, pass `{ nulls = "nil" }` as
// the second arg to `decode`.

fn install_json_global(lua: &Lua) -> mlua::Result<()> {
    let json = lua.create_table()?;
    // Sentinel: an empty, frozen-by-convention table. Identity comparison
    // (`v == json.null`) is the cheap and obvious check.
    let null_sentinel = lua.create_table()?;
    null_sentinel.set("__agentd_json_null", true)?;
    json.set("null", null_sentinel.clone())?;

    json.set(
        "is_null",
        lua.create_function(|_, v: Value| -> mlua::Result<bool> {
            Ok(match v {
                Value::Table(t) => {
                    t.get::<Option<bool>>("__agentd_json_null").ok().flatten() == Some(true)
                }
                _ => false,
            })
        })?,
    )?;

    json.set(
        "encode",
        lua.create_function(|_, v: Value| -> mlua::Result<String> {
            let j = lua_to_json_value(v)?;
            serde_json::to_string(&j).map_err(mlua::Error::external)
        })?,
    )?;

    let null_for_decode = null_sentinel.clone();
    json.set(
        "decode",
        lua.create_function(move |lua, args: MultiValue| -> mlua::Result<Value> {
            let mut it = args.into_iter();
            let s: String = lua.unpack(
                it.next()
                    .ok_or_else(|| mlua::Error::external("json.decode: input required"))?,
            )?;
            // Optional opts: { nulls = "sentinel" | "nil" }. Default = sentinel.
            let mut nulls_as_nil = false;
            if let Some(Value::Table(opts)) = it.next()
                && let Some(s) = opts.get::<Option<String>>("nulls")?
            {
                nulls_as_nil = s == "nil";
            }
            let j: serde_json::Value = serde_json::from_str(&s).map_err(mlua::Error::external)?;
            json_to_lua_value(lua, &j, &null_for_decode, nulls_as_nil)
        })?,
    )?;

    // Bare global, same family as `channel` / `service` / `async` / `sleep`.
    lua.globals().set("json", json)?;
    Ok(())
}

/// Walk a Lua value into a serde_json::Value. Honours the `json.null`
/// sentinel (a table containing `__agentd_json_null = true`) by emitting a
/// real JSON `null` — recursively, so nested sentinels also round-trip.
fn lua_to_json_value(v: Value) -> mlua::Result<serde_json::Value> {
    match v {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::json!(i)),
        Value::Number(f) => Ok(serde_json::json!(f)),
        Value::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        Value::Table(t) => {
            if t.get::<Option<bool>>("__agentd_json_null").ok().flatten() == Some(true) {
                return Ok(serde_json::Value::Null);
            }
            // Sequence vs map: lua arrays use positive integer keys 1..n.
            // We probe `#t` and verify each index resolves; otherwise treat as
            // map (string keys).
            let len = t.raw_len();
            let is_sequence = len > 0 && {
                let mut all = true;
                for i in 1..=len {
                    if matches!(t.raw_get::<Value>(i as i64)?, Value::Nil) {
                        all = false;
                        break;
                    }
                }
                all
            };
            if is_sequence {
                let mut arr = Vec::with_capacity(len);
                for i in 1..=len {
                    let item: Value = t.raw_get(i as i64)?;
                    arr.push(lua_to_json_value(item)?);
                }
                Ok(serde_json::Value::Array(arr))
            } else {
                let mut map = serde_json::Map::new();
                for pair in t.pairs::<Value, Value>() {
                    let (k, v) = pair?;
                    let key = match k {
                        Value::String(s) => s.to_str()?.to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) => n.to_string(),
                        other => {
                            return Err(mlua::Error::external(format!(
                                "json.encode: unsupported key type {}",
                                other.type_name()
                            )));
                        }
                    };
                    map.insert(key, lua_to_json_value(v)?);
                }
                Ok(serde_json::Value::Object(map))
            }
        }
        other => Err(mlua::Error::external(format!(
            "json.encode: cannot serialize {}",
            other.type_name()
        ))),
    }
}

/// Reverse of [`lua_to_json_value`]: walk a serde_json value into Lua,
/// substituting the supplied sentinel for JSON nulls (unless `nulls_as_nil`).
fn json_to_lua_value(
    lua: &Lua,
    j: &serde_json::Value,
    null_sentinel: &Table,
    nulls_as_nil: bool,
) -> mlua::Result<Value> {
    match j {
        serde_json::Value::Null => Ok(if nulls_as_nil {
            Value::Nil
        } else {
            Value::Table(null_sentinel.clone())
        }),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else {
                Ok(Value::Number(n.as_f64().unwrap_or(0.0)))
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.set(
                    i + 1,
                    json_to_lua_value(lua, item, null_sentinel, nulls_as_nil)?,
                )?;
            }
            Ok(Value::Table(t))
        }
        serde_json::Value::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                let lv = json_to_lua_value(lua, v, null_sentinel, nulls_as_nil)?;
                // When mapping nulls to nil, omit the key entirely (Lua
                // semantics) so callers see `obj.guild_id == nil` for null.
                if nulls_as_nil && matches!(lv, Value::Nil) {
                    continue;
                }
                t.set(k.as_str(), lv)?;
            }
            Ok(Value::Table(t))
        }
    }
}

// ---------- context.log ----------

fn build_log_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (name, level) in [
        ("trace", tracing::Level::TRACE),
        ("debug", tracing::Level::DEBUG),
        ("info", tracing::Level::INFO),
        ("warn", tracing::Level::WARN),
        ("error", tracing::Level::ERROR),
    ] {
        t.set(
            name,
            lua.create_function(move |_, msg: String| {
                log_at(level, &msg);
                Ok(())
            })?,
        )?;
    }
    Ok(t)
}

fn log_at(level: tracing::Level, msg: &str) {
    match level {
        tracing::Level::TRACE => tracing::trace!(target: "lua", "{msg}"),
        tracing::Level::DEBUG => tracing::debug!(target: "lua", "{msg}"),
        tracing::Level::INFO => tracing::info!(target: "lua", "{msg}"),
        tracing::Level::WARN => tracing::warn!(target: "lua", "{msg}"),
        tracing::Level::ERROR => tracing::error!(target: "lua", "{msg}"),
    }
}

// ---------- ctx.shell ----------

/// Translate an execution's effective grants into a child-process sandbox
/// policy. `fs.read`/`fs.write` slugs become readable/writable subtrees (globs
/// collapsed to the concrete ancestor dir), any `net:` grant flips coarse
/// network on, and `shell.unrestricted` opts the child out of the sandbox.
pub(crate) fn build_sandbox_policy(grants: &PermissionSet) -> SandboxPolicy {
    let mut policy = SandboxPolicy::default();
    for perm in grants.iter() {
        let slug = perm.as_str();
        let (domain, spec) = match slug.split_once(':') {
            Some((d, s)) => (d, Some(s)),
            None => (slug, None),
        };
        match domain {
            "fs.write" => {
                if let Some(s) = spec
                    && s.starts_with('/')
                {
                    policy.write_paths.push(concrete_ancestor(s));
                }
            }
            "fs.read" => {
                if let Some(s) = spec
                    && s.starts_with('/')
                {
                    policy.read_paths.push(concrete_ancestor(s));
                }
            }
            "net" => {
                policy.allow_net = true;
                policy.net_hosts.push(perm.clone());
            }
            "shell.unrestricted" => policy.unrestricted = true,
            _ => {}
        }
    }
    policy
}

fn shell_exec_binding(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut req = parse_shell_args(lua, args)?;
    // Per-binary scoping. A bare `shell.exec` grant authorizes any binary
    // (legacy/broad); a scoped `shell.exec:<bin>` (or holder wildcard like
    // `shell.exec:*`) authorizes just that binary. Allow if EITHER is held —
    // so granting only `shell.exec:git` restricts to git, while existing
    // `shell.exec` grants keep working unchanged.
    let bin_perm = Permission::new(format!("shell.exec:{}", req.bin));
    let bare_perm = Permission::new("shell.exec");
    let allowed = {
        let active = lua
            .app_data_ref::<ActiveContext>()
            .ok_or_else(|| mlua::Error::external("active context missing"))?;
        active.effective_grants.contains(&bare_perm) || active.effective_grants.contains(&bin_perm)
    };
    if !allowed {
        let active = lua
            .app_data_ref::<ActiveContext>()
            .ok_or_else(|| mlua::Error::external("active context missing"))?;
        let scoped = format!("shell.exec:{}", req.bin);
        return Err(mlua::Error::external(inline_denial(
            &active,
            &format!(
                "Lua tried to run binary `{}` but is missing `{}` (or broader `shell.exec`)",
                req.bin, scoped
            ),
            &[scoped],
        )));
    }

    // Confine the child to the execution's effective filesystem/network grants.
    // Must be attached before the request is wrapped into Op::Shell so the
    // scheduler carries the policy through to agentd_shell::exec.
    {
        let active = lua
            .app_data_ref::<ActiveContext>()
            .ok_or_else(|| mlua::Error::external("active context missing"))?;
        req.sandbox = Some(build_sandbox_policy(&active.effective_grants));
    }

    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::Shell(req));
    }
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|e| mlua::Error::external(format!("no tokio runtime: {e}")))?;
    let result = handle
        .block_on(shell_exec(req))
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    let out = lua.create_table()?;
    out.set("exit_code", result.exit_code)?;
    out.set("stdout", result.stdout)?;
    out.set("stderr", result.stderr)?;
    Ok(Value::Table(out))
}

fn parse_shell_args(_lua: &Lua, args: MultiValue) -> mlua::Result<ExecRequest> {
    let mut iter = args.into_iter();
    let first = iter
        .next()
        .ok_or_else(|| mlua::Error::external("context.shell.exec: bin required"))?;

    match first {
        Value::Table(t) => {
            let bin: String = t
                .get("bin")
                .map_err(|_| mlua::Error::external("shell.exec: `bin` required"))?;
            let args: Vec<String> = match t.get::<Value>("args")? {
                Value::Nil => Vec::new(),
                Value::Table(a) => a.sequence_values::<String>().collect::<mlua::Result<_>>()?,
                other => {
                    return Err(mlua::Error::external(format!(
                        "shell.exec: `args` must be array, got {}",
                        other.type_name()
                    )));
                }
            };
            let cwd: Option<String> = t.get("cwd").ok();
            let stdin: Option<String> = t.get("stdin").ok();
            let separate_stderr: bool = t.get::<Option<bool>>("separate_stderr")?.unwrap_or(true);
            Ok(ExecRequest {
                bin,
                args,
                cwd: cwd.map(Into::into),
                stdin,
                separate_stderr,
                sandbox: None,
            })
        }
        Value::String(s) => {
            let bin = s.to_str()?.to_string();
            let args = match iter.next() {
                None | Some(Value::Nil) => Vec::new(),
                Some(Value::Table(a)) => a
                    .sequence_values::<String>()
                    .collect::<mlua::Result<Vec<_>>>()?,
                Some(other) => {
                    return Err(mlua::Error::external(format!(
                        "shell.exec: argv must be a table, got {}",
                        other.type_name()
                    )));
                }
            };
            let opts = iter.next();
            let (cwd, stdin, separate_stderr) = match opts {
                None | Some(Value::Nil) => (None, None, true),
                Some(Value::Table(t)) => {
                    let cwd: Option<String> = t.get("cwd").ok();
                    let stdin: Option<String> = t.get("stdin").ok();
                    let sep: bool = t.get::<Option<bool>>("separate_stderr")?.unwrap_or(true);
                    (cwd, stdin, sep)
                }
                Some(other) => {
                    return Err(mlua::Error::external(format!(
                        "shell.exec: opts must be table, got {}",
                        other.type_name()
                    )));
                }
            };
            Ok(ExecRequest {
                bin,
                args,
                cwd: cwd.map(Into::into),
                stdin,
                separate_stderr,
                sandbox: None,
            })
        }
        other => Err(mlua::Error::external(format!(
            "shell.exec: first arg must be bin name or table, got {}",
            other.type_name()
        ))),
    }
}

// ---------- context.fs ----------

fn build_fs_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("read", lua.create_function(fs_read_binding)?)?;
    t.set("write", lua.create_function(fs_write_binding)?)?;
    t.set("append", lua.create_function(fs_append_binding)?)?;
    t.set("exists", lua.create_function(fs_exists_binding)?)?;
    t.set("stat", lua.create_function(fs_stat_binding)?)?;
    t.set("list_dir", lua.create_function(fs_list_dir_binding)?)?;
    t.set("remove", lua.create_function(fs_remove_binding)?)?;
    Ok(t)
}

fn abs_path(path: String) -> std::path::PathBuf {
    let p = std::path::PathBuf::from(&path);
    if p.is_absolute() {
        p
    } else {
        // Resolve relative paths against CWD so the permission slug we check
        // matches a stable absolute path.
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// Resolve a user-supplied path to the absolute, symlink-free path used BOTH
/// for the `fs.*` permission slug and the I/O that follows. Resolving before
/// the slug is derived is what stops a `..` segment or a symlink from pointing
/// the real operation somewhere a path-scoped grant would never have allowed
/// (e.g. `fs.write:/tmp/**` writing through `/tmp/link -> /etc/passwd`).
fn resolve_path(path: String) -> std::path::PathBuf {
    let p = abs_path(path);
    if let Ok(canon) = p.canonicalize() {
        return canon;
    }
    // Target may not exist yet (writing a new file): canonicalize the parent
    // chain so symlinks / `..` there still collapse, then re-attach the leaf.
    if let (Some(parent), Some(name)) = (p.parent(), p.file_name())
        && let Ok(canon_parent) = parent.canonicalize()
    {
        return canon_parent.join(name);
    }
    // Nothing on disk to resolve against — strip `.`/`..` lexically so a
    // traversal can't survive into the slug even on a fully novel path.
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn block_on<F: std::future::Future<Output = T>, T>(fut: F) -> mlua::Result<T> {
    tokio::runtime::Handle::try_current()
        .map_err(|e| mlua::Error::external(format!("no tokio runtime: {e}")))
        .map(|h| h.block_on(fut))
}

fn fs_read_binding(lua: &Lua, path: String) -> mlua::Result<String> {
    let p = resolve_path(path);
    check_permission_inline(lua, &Permission::new(format!("fs.read:{}", p.display())))?;
    block_on(fs::read_to_string(&p))?.map_err(|e| mlua::Error::external(e.to_string()))
}

fn fs_write_binding(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    let mut it = args.into_iter();
    let path: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("fs.write: path required"))?,
    )?;
    let content: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("fs.write: content required"))?,
    )?;
    let p = resolve_path(path);
    check_permission_inline(lua, &Permission::new(format!("fs.write:{}", p.display())))?;
    block_on(fs::write(&p, content.as_bytes()))?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    Ok(())
}

fn fs_append_binding(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    let mut it = args.into_iter();
    let path: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("fs.append: path required"))?,
    )?;
    let content: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("fs.append: content required"))?,
    )?;
    let p = resolve_path(path);
    check_permission_inline(lua, &Permission::new(format!("fs.write:{}", p.display())))?;
    block_on(fs::append(&p, content.as_bytes()))?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    Ok(())
}

fn fs_exists_binding(lua: &Lua, path: String) -> mlua::Result<bool> {
    let p = resolve_path(path);
    // `exists` reveals presence; gate as read.
    check_permission_inline(lua, &Permission::new(format!("fs.read:{}", p.display())))?;
    block_on(fs::exists(&p))
}

fn fs_stat_binding(lua: &Lua, path: String) -> mlua::Result<Table> {
    let p = resolve_path(path);
    check_permission_inline(lua, &Permission::new(format!("fs.read:{}", p.display())))?;
    let st = block_on(fs::stat(&p))?.map_err(|e| mlua::Error::external(e.to_string()))?;
    let t = lua.create_table()?;
    t.set("path", st.path.to_string_lossy().into_owned())?;
    t.set(
        "kind",
        match st.kind {
            fs::EntryKind::File => "file",
            fs::EntryKind::Dir => "dir",
            fs::EntryKind::Symlink => "symlink",
            fs::EntryKind::Other => "other",
        },
    )?;
    t.set("size", st.size)?;
    t.set("readonly", st.readonly)?;
    if let Some(m) = st.modified_unix {
        t.set("modified_unix", m)?;
    }
    Ok(t)
}

fn fs_list_dir_binding(lua: &Lua, path: String) -> mlua::Result<Table> {
    let p = resolve_path(path);
    check_permission_inline(lua, &Permission::new(format!("fs.read:{}", p.display())))?;
    let entries = block_on(fs::list_dir(&p))?.map_err(|e| mlua::Error::external(e.to_string()))?;
    let arr = lua.create_table()?;
    for (i, e) in entries.into_iter().enumerate() {
        let row = lua.create_table()?;
        row.set("name", e.name)?;
        row.set("path", e.path.to_string_lossy().into_owned())?;
        row.set(
            "kind",
            match e.kind {
                fs::EntryKind::File => "file",
                fs::EntryKind::Dir => "dir",
                fs::EntryKind::Symlink => "symlink",
                fs::EntryKind::Other => "other",
            },
        )?;
        arr.set(i + 1, row)?;
    }
    Ok(arr)
}

fn fs_remove_binding(lua: &Lua, path: String) -> mlua::Result<()> {
    let p = resolve_path(path);
    check_permission_inline(lua, &Permission::new(format!("fs.write:{}", p.display())))?;
    block_on(fs::remove_file(&p))?.map_err(|e| mlua::Error::external(e.to_string()))?;
    Ok(())
}

// ---------- context.http ----------

fn build_http_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "get",
        yieldable_wrap(lua, lua.create_function(http_get_binding)?)?,
    )?;
    t.set(
        "post",
        yieldable_wrap(lua, lua.create_function(http_post_binding)?)?,
    )?;
    t.set(
        "request",
        yieldable_wrap(lua, lua.create_function(http_request_binding)?)?,
    )?;
    Ok(t)
}

fn http_get_binding(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut it = args.into_iter();
    let url: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("http.get: url required"))?,
    )?;
    let opts = it.next();
    let mut req = parse_http_opts(lua, opts)?;
    req.method = "GET".into();
    req.url = url;
    do_http(lua, req)
}

fn http_post_binding(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut it = args.into_iter();
    let url: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("http.post: url required"))?,
    )?;
    let mut req = HttpRequest {
        method: "POST".into(),
        url,
        ..Default::default()
    };
    // Second arg: body (string) or JSON value (table → json).
    if let Some(v) = it.next() {
        match v {
            Value::Nil => {}
            Value::String(s) => req.body = Some(s.to_str()?.to_string()),
            Value::Table(_) => {
                let json: serde_json::Value = lua
                    .from_value(v)
                    .map_err(|e| mlua::Error::external(format!("http.post: json: {e}")))?;
                req.json = Some(json);
            }
            other => {
                return Err(mlua::Error::external(format!(
                    "http.post: body must be string or table, got {}",
                    other.type_name()
                )));
            }
        }
    }
    // Third arg: opts table (headers, timeout_ms).
    if let Some(opts) = it.next() {
        let extra = parse_http_opts(lua, Some(opts))?;
        for (k, v) in extra.headers {
            req.headers.insert(k, v);
        }
        if let Some(t) = extra.timeout_ms {
            req.timeout_ms = Some(t);
        }
    }
    do_http(lua, req)
}

fn http_request_binding(lua: &Lua, t: Table) -> mlua::Result<Value> {
    let url: String = t
        .get("url")
        .map_err(|_| mlua::Error::external("http.request: `url` required"))?;
    let method: String = t
        .get::<Option<String>>("method")?
        .unwrap_or_else(|| "GET".into());
    let mut req = parse_http_opts(lua, Some(Value::Table(t)))?;
    req.url = url;
    req.method = method;
    do_http(lua, req)
}

fn parse_http_opts(lua: &Lua, opts: Option<Value>) -> mlua::Result<HttpRequest> {
    let mut req = HttpRequest::default();
    match opts {
        None | Some(Value::Nil) => {}
        Some(Value::Table(t)) => {
            // headers
            if let Ok(Value::Table(h)) = t.get::<Value>("headers") {
                for pair in h.pairs::<String, String>() {
                    let (k, v) = pair?;
                    req.headers.insert(k, v);
                }
            }
            if let Some(b) = t.get::<Option<String>>("body")? {
                req.body = Some(b);
            }
            match t.get::<Value>("json") {
                Ok(Value::Nil) | Err(_) => {}
                Ok(v) => {
                    let j: serde_json::Value = lua
                        .from_value(v)
                        .map_err(|e| mlua::Error::external(format!("http: json: {e}")))?;
                    req.json = Some(j);
                }
            }
            if let Some(t) = t.get::<Option<u64>>("timeout_ms")? {
                req.timeout_ms = Some(t);
            }
        }
        Some(other) => {
            return Err(mlua::Error::external(format!(
                "http opts must be table, got {}",
                other.type_name()
            )));
        }
    }
    Ok(req)
}

fn do_http(lua: &Lua, req: HttpRequest) -> mlua::Result<Value> {
    let host = host_of(&req.url).map_err(|e| mlua::Error::external(e.to_string()))?;
    check_permission_inline(lua, &Permission::new(format!("net:{host}")))?;
    if scheduler::is_in_coroutine(lua) {
        // Yield to the scheduler so other coroutines / async tasks can make
        // progress while reqwest does its thing.
        return scheduler::build_marker(lua, scheduler::Op::Http(req));
    }
    // Top-level call (e.g. from init.lua) — no coroutine to yield from, so
    // block the current thread.
    let resp = block_on(http_send(req))?.map_err(|e| mlua::Error::external(e.to_string()))?;
    let t = lua.create_table()?;
    t.set("status", resp.status)?;
    t.set("body", resp.body)?;
    let h = lua.create_table()?;
    for (k, v) in resp.headers {
        h.set(k, v)?;
    }
    t.set("headers", h)?;
    Ok(Value::Table(t))
}

// ---------- ctx.secret ----------

fn build_secret_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("get", lua.create_function(auth_get_binding)?)?;
    t.set("set", lua.create_function(auth_set_binding)?)?;
    t.set("delete", lua.create_function(auth_delete_binding)?)?;
    t.set("exists", lua.create_function(auth_exists_binding)?)?;
    t.set("list", lua.create_function(auth_list_binding)?)?;
    Ok(t)
}

fn with_secrets<F, T>(lua: &Lua, f: F) -> mlua::Result<T>
where
    F: FnOnce(&Arc<dyn SecretStore>) -> mlua::Result<T>,
{
    let h = lua
        .app_data_ref::<SecretsHolder>()
        .ok_or_else(|| mlua::Error::external("auth: secrets holder missing"))?;
    let store =
        h.0.as_ref()
            .ok_or_else(|| mlua::Error::external("auth: no secrets backend configured"))?;
    f(store)
}

// ---------- ctx.memory ----------
//
// Durable namespaced kv. `ctx.memory.create(ns)` returns a handle binding the
// namespace; each operation gates inline on `memory.read:<ns>` /
// `memory.write:<ns>` (fs-style) and routes through the injected MemoryStore.
// Blocking like `ctx.fs`/`ctx.secret` — the C internals return directly.

fn with_memory<F, T>(lua: &Lua, f: F) -> mlua::Result<T>
where
    F: FnOnce(&Arc<dyn MemoryStore>) -> mlua::Result<T>,
{
    let h = lua
        .app_data_ref::<MemoryHolder>()
        .ok_or_else(|| mlua::Error::external("memory: holder missing"))?;
    let store =
        h.0.as_ref()
            .ok_or_else(|| mlua::Error::external("memory: no memory backend configured"))?;
    f(store)
}

/// Parse exactly `(ns, key)` from a MultiValue.
fn two_strings(lua: &Lua, args: MultiValue, what: &str) -> mlua::Result<(String, String)> {
    let mut it = args.into_iter();
    let a: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external(format!("{what}: ns required")))?,
    )?;
    let b: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external(format!("{what}: key required")))?,
    )?;
    Ok((a, b))
}

fn mem_get_binding(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let (ns, key) = two_strings(lua, args, "memory:get")?;
    check_permission_inline(lua, &Permission::new(format!("memory.read:{ns}")))?;
    let bytes = with_memory(lua, |s| {
        s.get(&ns, &key)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })?;
    match bytes {
        None => Ok(Value::Nil),
        Some(b) => {
            let j: serde_json::Value = serde_json::from_slice(&b).map_err(mlua::Error::external)?;
            lua.to_value(&j)
        }
    }
}

fn mem_set_binding(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    let mut it = args.into_iter();
    let ns: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("memory:set: ns required"))?,
    )?;
    let key: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("memory:set: key required"))?,
    )?;
    let v = it
        .next()
        .ok_or_else(|| mlua::Error::external("memory:set: value required"))?;
    check_permission_inline(lua, &Permission::new(format!("memory.write:{ns}")))?;
    let j: serde_json::Value = lua.from_value(v)?;
    let bytes = serde_json::to_vec(&j).map_err(mlua::Error::external)?;
    with_memory(lua, |s| {
        s.set(&ns, &key, &bytes)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn mem_delete_binding(lua: &Lua, args: MultiValue) -> mlua::Result<bool> {
    let (ns, key) = two_strings(lua, args, "memory:delete")?;
    check_permission_inline(lua, &Permission::new(format!("memory.write:{ns}")))?;
    with_memory(lua, |s| {
        s.delete(&ns, &key)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn mem_exists_binding(lua: &Lua, args: MultiValue) -> mlua::Result<bool> {
    let (ns, key) = two_strings(lua, args, "memory:exists")?;
    check_permission_inline(lua, &Permission::new(format!("memory.read:{ns}")))?;
    with_memory(lua, |s| {
        s.exists(&ns, &key)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn mem_keys_binding(lua: &Lua, ns: String) -> mlua::Result<Table> {
    check_permission_inline(lua, &Permission::new(format!("memory.read:{ns}")))?;
    let names = with_memory(lua, |s| {
        s.keys(&ns)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })?;
    names_to_table(lua, names)
}

fn mem_clear_binding(lua: &Lua, ns: String) -> mlua::Result<()> {
    check_permission_inline(lua, &Permission::new(format!("memory.write:{ns}")))?;
    with_memory(lua, |s| {
        s.clear(&ns)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn build_memory_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let get = lua.create_function(mem_get_binding)?;
    let set = lua.create_function(mem_set_binding)?;
    let del = lua.create_function(mem_delete_binding)?;
    let exists = lua.create_function(mem_exists_binding)?;
    let keys = lua.create_function(mem_keys_binding)?;
    let clear = lua.create_function(mem_clear_binding)?;
    let ctor: Function = lua
        .load(
            r#"
        local get, set, del, exists, keys, clear = ...
        return function(ns)
          if type(ns) ~= "string" or ns == "" then
            error("ctx.memory.create: ns must be a non-empty string", 0)
          end
          return {
            _ns = ns,
            -- `default` is returned when the key is absent (nil from the
            -- store). A stored JSON null still comes back as json.null.
            get    = function(self, k, default)
              local v = get(self._ns, k)
              if v == nil then return default end
              return v
            end,
            set    = function(self, k, v) return set(self._ns, k, v) end,
            delete = function(self, k)    return del(self._ns, k) end,
            exists = function(self, k)    return exists(self._ns, k) end,
            keys   = function(self)       return keys(self._ns) end,
            clear  = function(self)       return clear(self._ns) end,
          }
        end
    "#,
        )
        .set_name("ctx.memory.create")
        .call((get, set, del, exists, keys, clear))?;
    t.set("create", ctor)?;
    Ok(t)
}

fn auth_get_binding(lua: &Lua, key: String) -> mlua::Result<String> {
    check_permission_inline(lua, &Permission::new(format!("secret:{key}")))?;
    with_secrets(lua, |s| {
        s.get(&key)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn auth_set_binding(lua: &Lua, args: MultiValue) -> mlua::Result<()> {
    let mut it = args.into_iter();
    let key: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("auth.set: key required"))?,
    )?;
    let value: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("auth.set: value required"))?,
    )?;
    check_permission_inline(lua, &Permission::new(format!("secret:{key}")))?;
    with_secrets(lua, |s| {
        s.set(&key, &value)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn auth_delete_binding(lua: &Lua, key: String) -> mlua::Result<()> {
    check_permission_inline(lua, &Permission::new(format!("secret:{key}")))?;
    with_secrets(lua, |s| {
        s.delete(&key)
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn auth_exists_binding(lua: &Lua, key: String) -> mlua::Result<bool> {
    // Same gate as `get` — existence of a key is itself information about
    // that key, but never exposes the value to Lua.
    check_permission_inline(lua, &Permission::new(format!("secret:{key}")))?;
    with_secrets(lua, |s| {
        s.try_get(&key)
            .map(|v| v.is_some())
            .map_err(|e| mlua::Error::external(e.to_string()))
    })
}

fn auth_list_binding(lua: &Lua, _args: MultiValue) -> mlua::Result<Table> {
    // Listing exposes which keys exist; gate as `secret:*` (anything).
    check_permission_inline(lua, &Permission::new("secret:*"))?;
    let names = with_secrets(lua, |s| {
        s.list().map_err(|e| mlua::Error::external(e.to_string()))
    })?;
    let t = lua.create_table()?;
    for (i, n) in names.into_iter().enumerate() {
        t.set(i + 1, n)?;
    }
    Ok(t)
}

// ---------- context.ai ----------

fn build_ai_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "ask",
        yieldable_wrap(lua, lua.create_function(ai_ask_binding)?)?,
    )?;
    t.set(
        "complete",
        yieldable_wrap(lua, lua.create_function(ai_complete_binding)?)?,
    )?;
    t.set("providers", lua.create_function(ai_providers_binding)?)?;
    Ok(t)
}

fn ai_providers_binding(lua: &Lua, _args: MultiValue) -> mlua::Result<Table> {
    let holder = lua
        .app_data_ref::<AiHolder>()
        .ok_or_else(|| mlua::Error::external("ai: holder missing"))?;
    let t = lua.create_table()?;
    let mut names: Vec<&String> = holder.providers.keys().collect();
    names.sort();
    for (i, n) in names.into_iter().enumerate() {
        t.set(i + 1, n.clone())?;
    }
    Ok(t)
}

fn ai_ask_binding(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut it = args.into_iter();
    let prompt: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("ai.ask: prompt required"))?,
    )?;
    let opts = it.next();
    let req = build_completion_request(lua, prompt, opts)?;
    do_ai(lua, req)
}

fn ai_complete_binding(lua: &Lua, opts: Table) -> mlua::Result<Value> {
    let prompt: Option<String> = opts.get::<Option<String>>("prompt")?;
    let req = build_completion_request(lua, prompt.unwrap_or_default(), Some(Value::Table(opts)))?;
    do_ai(lua, req)
}

fn build_completion_request(
    lua: &Lua,
    prompt: String,
    opts: Option<Value>,
) -> mlua::Result<(CompletionRequest, String)> {
    let mut req = CompletionRequest::default();
    if !prompt.is_empty() {
        req.prompt = Some(prompt);
    }
    let mut model_raw: Option<String> = None;
    if let Some(Value::Table(t)) = opts {
        if t.get::<Option<String>>("provider")?.is_some() {
            return Err(mlua::Error::external(
                "ai: `provider` opt is gone — use `model = \"<provider>/<model>\"`",
            ));
        }
        if let Some(s) = t.get::<Option<String>>("system")? {
            req.system = Some(s);
        }
        if let Some(m) = t.get::<Option<String>>("model")? {
            model_raw = Some(m);
        }
        if let Some(mt) = t.get::<Option<u32>>("max_tokens")? {
            req.max_tokens = Some(mt);
        }
        if let Some(p) = t.get::<Option<String>>("prompt")?
            && req.prompt.is_none()
        {
            req.prompt = Some(p);
        }
        if let Ok(Value::Table(msgs)) = t.get::<Value>("messages") {
            let mut out = Vec::new();
            for pair in msgs.sequence_values::<Table>() {
                let m = pair?;
                let role_s: String = m.get("role")?;
                let role = match role_s.as_str() {
                    "system" => Role::System,
                    "user" => Role::User,
                    "assistant" => Role::Assistant,
                    other => {
                        return Err(mlua::Error::external(format!("ai: unknown role `{other}`")));
                    }
                };
                let content: String = m.get("content")?;
                out.push(Message {
                    role,
                    content,
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
            req.messages = out;
        }
    }
    // Split model = "<provider>/<id>". When the prefix is omitted, fall back
    // to the host's default provider name; that lets short scripts say
    // `model = "claude-opus-4-7"` if they're fine with whatever default the
    // daemon was configured with.
    let (provider_name, model_id) = match model_raw.as_deref() {
        Some(s) => {
            let (p, m) = agentd_ai::registry::parse_model(s);
            (p.map(|x| x.to_string()), Some(m.to_string()))
        }
        None => (None, None),
    };
    let resolved_provider = {
        let holder = lua
            .app_data_ref::<AiHolder>()
            .ok_or_else(|| mlua::Error::external("ai: holder missing"))?;
        match provider_name {
            Some(n) => n,
            None => holder
                .default
                .clone()
                .ok_or_else(|| mlua::Error::external("ai: no default provider configured"))?,
        }
    };
    req.model = model_id.filter(|s| !s.is_empty());
    Ok((req, resolved_provider))
}

fn do_ai(lua: &Lua, (req, provider_name): (CompletionRequest, String)) -> mlua::Result<Value> {
    check_permission_inline(lua, &Permission::new(format!("ai:{provider_name}")))?;
    let provider = {
        let holder = lua
            .app_data_ref::<AiHolder>()
            .ok_or_else(|| mlua::Error::external("ai: holder missing"))?;
        holder.providers.get(&provider_name).cloned()
    };
    let provider = provider.ok_or_else(|| {
        mlua::Error::external(format!("ai: provider `{provider_name}` not registered"))
    })?;
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(
            lua,
            scheduler::Op::Ai {
                provider_name,
                provider,
                request: req,
            },
        );
    }
    let resp =
        block_on(provider.complete(req))?.map_err(|e| mlua::Error::external(e.to_string()))?;
    let t = lua.create_table()?;
    t.set("text", resp.text)?;
    if let Some(m) = resp.model {
        t.set("model", m)?;
    }
    if let Some(sr) = resp.stop_reason {
        t.set("stop_reason", sr)?;
    }
    t.set("provider", provider_name)?;
    Ok(Value::Table(t))
}

// ---------- context.ws ----------
//
// The Lua-facing handle is a *table* (not raw userdata) so that
// `:send` / `:send_binary` / `:recv` / `:close` can be Lua-closure wrappers
// around C internals. The closure yields the `OpMarker` userdata via
// `coroutine.yield`, which is legal because the yield happens in a Lua
// frame. The actual `Arc<WsConnection>` lives in a userdata stored under
// the table's `_conn` field.

/// Raw userdata wrapping a connection handle. No methods — methods live on
/// the Lua-side table built in `build_ws_table`.
struct WsHandle {
    conn: Arc<WsConnection>,
}

impl mlua::UserData for WsHandle {}

fn build_ws_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;

    let connect_internal = lua.create_function(ws_connect_internal)?;
    let send_internal = lua.create_function(ws_send_internal)?;
    let send_binary_internal = lua.create_function(ws_send_binary_internal)?;
    let recv_internal = lua.create_function(ws_recv_internal)?;
    let close_internal = lua.create_function(ws_close_internal)?;
    let is_closed_internal = lua.create_function(ws_is_closed_internal)?;
    let url_internal = lua.create_function(ws_url_internal)?;
    let bytes_table_to_string = lua.create_function(ws_bytes_table_to_string)?;

    // Lua-side constructor. Builds the table handle and wires up methods as
    // Lua closures that call the C internals, then yield on userdata-shaped
    // returns. Same pattern as channels.
    let chunk_src = r#"
        local connect_int, send_int, sb_int, recv_int, close_int, ic_int, url_int, bytes_to_str = ...

        local function pass_through(v)
          if type(v) == "userdata" then
            v = coroutine.yield(v)
            if type(v) == "table" and v.ok == false then
              error(v.error or "ws error", 0)
            end
          end
          return v
        end

        local function normalize_frame(frame)
          -- When `recv` yielded, the scheduler returned a binary frame as
          -- `binary_bytes = {b1, b2, ...}`. Convert to a Lua string under
          -- `binary` so user code sees the same shape regardless of path.
          if type(frame) == "table"
             and frame.kind == "binary"
             and frame.binary == nil
             and frame.binary_bytes ~= nil then
            frame.binary = bytes_to_str(frame.binary_bytes)
            frame.binary_bytes = nil
          end
          return frame
        end

        local function build(conn)
          return {
            _conn = conn,
            send = function(self, msg)         return pass_through(send_int(self._conn, msg)) end,
            send_binary = function(self, b)    return pass_through(sb_int(self._conn, b)) end,
            recv = function(self, timeout_ms)  return normalize_frame(pass_through(recv_int(self._conn, timeout_ms))) end,
            close = function(self)             return pass_through(close_int(self._conn)) end,
            is_closed = function(self)         return ic_int(self._conn) end,
            url = function(self)               return url_int(self._conn) end,
          }
        end

        return {
          connect = function(url)
            -- Connect is short and one-shot; we keep it block_on. If the
            -- handshake ever needs to be yieldable, swap the internal here.
            local conn = connect_int(url)
            return build(conn)
          end,
        }
    "#;
    let ws_namespace: Table = lua.load(chunk_src).set_name("ws ctor").call((
        connect_internal,
        send_internal,
        send_binary_internal,
        recv_internal,
        close_internal,
        is_closed_internal,
        url_internal,
        bytes_table_to_string,
    ))?;
    for pair in ws_namespace.pairs::<Value, Value>() {
        let (k, v) = pair?;
        t.set(k, v)?;
    }
    Ok(t)
}

fn ws_connect_internal(lua: &Lua, url: String) -> mlua::Result<mlua::AnyUserData> {
    let host = ws_host_of(&url).map_err(|e| mlua::Error::external(e.to_string()))?;
    check_permission_inline(lua, &Permission::new(format!("net:{host}")))?;
    let conn = block_on(async move { WsConnection::connect(&url).await })?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    lua.create_userdata(WsHandle {
        conn: Arc::new(conn),
    })
}

fn ws_send_internal(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut it = args.into_iter();
    let ud: mlua::AnyUserData = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("ws:send: handle required"))?,
    )?;
    let msg: String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("ws:send: message required"))?,
    )?;
    let conn = ud.borrow::<WsHandle>()?.conn.clone();
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::WsSendText { conn, msg });
    }
    block_on(async move { conn.send_text(&msg).await })?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    Ok(Value::Nil)
}

fn ws_send_binary_internal(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut it = args.into_iter();
    let ud: mlua::AnyUserData = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("ws:send_binary: handle required"))?,
    )?;
    let bytes: mlua::String = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("ws:send_binary: bytes required"))?,
    )?;
    let bytes_vec: Vec<u8> = bytes.as_bytes().to_vec();
    let conn = ud.borrow::<WsHandle>()?.conn.clone();
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(
            lua,
            scheduler::Op::WsSendBinary {
                conn,
                bytes: bytes_vec,
            },
        );
    }
    block_on(async move { conn.send_binary(bytes_vec).await })?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    Ok(Value::Nil)
}

fn ws_recv_internal(lua: &Lua, args: MultiValue) -> mlua::Result<Value> {
    let mut it = args.into_iter();
    let ud: mlua::AnyUserData = lua.unpack(
        it.next()
            .ok_or_else(|| mlua::Error::external("ws:recv: handle required"))?,
    )?;
    let timeout_ms: Option<u64> = match it.next() {
        None | Some(Value::Nil) => None,
        Some(v) => Some(lua.unpack(v)?),
    };
    let timeout = timeout_ms.map(std::time::Duration::from_millis);
    let conn = ud.borrow::<WsHandle>()?.conn.clone();
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::WsRecv { conn, timeout });
    }
    let frame = block_on(async move { conn.recv(timeout).await })?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    let t = lua.create_table()?;
    match frame {
        WsFrame::Text(s) => {
            t.set("kind", "text")?;
            t.set("text", s)?;
        }
        WsFrame::Binary(b) => {
            t.set("kind", "binary")?;
            t.set("binary", lua.create_string(&b)?)?;
        }
        WsFrame::Close { code, reason } => {
            t.set("kind", "close")?;
            t.set("code", code)?;
            t.set("reason", reason)?;
        }
    }
    Ok(Value::Table(t))
}

fn ws_close_internal(lua: &Lua, ud: mlua::AnyUserData) -> mlua::Result<Value> {
    let conn = ud.borrow::<WsHandle>()?.conn.clone();
    if scheduler::is_in_coroutine(lua) {
        return scheduler::build_marker(lua, scheduler::Op::WsClose { conn });
    }
    block_on(async move { conn.close().await })?
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    Ok(Value::Nil)
}

fn ws_is_closed_internal(_lua: &Lua, ud: mlua::AnyUserData) -> mlua::Result<bool> {
    let conn = ud.borrow::<WsHandle>()?.conn.clone();
    block_on(async move { conn.is_closed().await })
}

fn ws_url_internal(_lua: &Lua, ud: mlua::AnyUserData) -> mlua::Result<String> {
    let conn = ud.borrow::<WsHandle>()?.conn.clone();
    Ok(conn.url().to_string())
}

/// Convert a Lua sequence of byte-sized integers (`{b1, b2, ...}`) into a
/// Lua string. Used to bridge the scheduler's JSON-shaped binary frame
/// payload back into a raw-byte string that's convenient for Lua callers.
fn ws_bytes_table_to_string(lua: &Lua, t: Table) -> mlua::Result<mlua::String> {
    let mut bytes: Vec<u8> = Vec::new();
    for v in t.sequence_values::<i64>() {
        let n = v?;
        if !(0..=255).contains(&n) {
            return Err(mlua::Error::external(format!(
                "ws: binary byte out of range: {n}"
            )));
        }
        bytes.push(n as u8);
    }
    lua.create_string(&bytes)
}

// ---------- ctx.call / ctx.tools internals ----------
// The `ctx.call` Lua wrapper + `ctx.tools` listing are assembled in
// `build_and_store_ctx`; these C internals back them: `tools_resolve_binding`
// resolves the handler + runs permission checks + pushes the call chain,
// `tools_pop_chain_binding` pops it, `tools_list_binding` lists action names.

fn tools_resolve_binding(lua: &Lua, args: MultiValue) -> mlua::Result<Function> {
    let mut iter = args.into_iter();
    let name: String = match iter.next() {
        Some(v) => lua.unpack(v)?,
        None => return Err(mlua::Error::external("context.tools.call: name required")),
    };
    // args are already validated as a serde value by the Lua wrapper before
    // it calls us; we only need the name + meta for the permission check.
    let _ = iter.next();

    let (handler, action_meta) = {
        let catalog = lua
            .app_data_ref::<SharedCatalog>()
            .ok_or_else(|| mlua::Error::external("scripting catalog missing"))?;
        let cat = catalog
            .read()
            .map_err(|e| mlua::Error::external(e.to_string()))?;
        let key = cat
            .actions
            .get(&name)
            .ok_or_else(|| mlua::Error::external(format!("action `{name}` not registered")))?;
        let inner = cat.action_meta.get(&name).cloned().unwrap_or_default();
        let func: Function = lua
            .registry_value(key)
            .map_err(|e| mlua::Error::external(e.to_string()))?;
        (func, inner)
    };

    {
        let active = lua
            .app_data_ref::<ActiveContext>()
            .ok_or_else(|| mlua::Error::external("active context missing"))?;
        if action_meta.confirm {
            return Err(mlua::Error::external(format!(
                "action `{name}` requires confirmation; cannot be invoked via context.tools.call"
            )));
        }
        for req in &action_meta.requires {
            let p = Permission::new(req);
            if !active.effective_grants.contains(&p) {
                return Err(mlua::Error::external(inline_denial(
                    &active,
                    &format!(
                        "Lua tried to call action `{name}` but that action requires `{}`",
                        p.as_str()
                    ),
                    &[p.as_str().to_string()],
                )));
            }
        }
    }
    {
        let mut active = lua
            .app_data_mut::<ActiveContext>()
            .ok_or_else(|| mlua::Error::external("active context missing"))?;
        active.call_chain.push(name);
    }
    Ok(handler)
}

fn tools_pop_chain_binding(lua: &Lua, _: MultiValue) -> mlua::Result<()> {
    if let Some(mut active) = lua.app_data_mut::<ActiveContext>() {
        active.call_chain.pop();
    }
    Ok(())
}

// ---------- ctx.caller ----------
//
// Read-only view of the identity the permission engine evaluated for this
// invocation. Fields resolve live against the per-resume `ActiveContext`, so
// the single shared table stays correct across concurrent coroutines.
//
//   ctx.caller.interface  -- "ws", "telegram", … or nil
//   ctx.caller.runner     -- runner name or nil
//   ctx.caller.service    -- service name or nil
//   ctx.caller.session    -- per-connection/session id or nil
//   ctx.caller.user       -- end-user id or nil

fn build_caller_table(lua: &Lua) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set(
        "__index",
        lua.create_function(|lua, (_t, key): (Table, String)| {
            let active = lua
                .app_data_ref::<ActiveContext>()
                .ok_or_else(|| mlua::Error::external("active context missing"))?;
            let c = &active.caller;
            let v = match key.as_str() {
                "interface" => c.interface.as_ref().map(|x| x.as_str().to_string()),
                "runner" => c.runner.as_ref().map(|x| x.as_str().to_string()),
                "service" => c.service.as_ref().map(|x| x.as_str().to_string()),
                "session" => c.session.as_ref().map(|x| x.as_str().to_string()),
                "user" => c.user.as_ref().map(|x| x.as_str().to_string()),
                "execution" => c.execution.as_ref().map(|x| x.as_str().to_string()),
                _ => None,
            };
            match v {
                Some(s) => Ok(Value::String(lua.create_string(&s)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    meta.set(
        "__newindex",
        lua.create_function(|_, _: MultiValue| -> mlua::Result<()> {
            Err(mlua::Error::external("ctx.caller is read-only"))
        })?,
    )?;
    t.set_metatable(Some(meta))?;
    Ok(t)
}

fn tools_list_binding(lua: &Lua, _args: MultiValue) -> mlua::Result<Table> {
    let catalog = lua
        .app_data_ref::<SharedCatalog>()
        .ok_or_else(|| mlua::Error::external("scripting catalog missing"))?;
    let cat = catalog
        .read()
        .map_err(|e| mlua::Error::external(e.to_string()))?;
    let mut names: Vec<String> = cat.actions.keys().cloned().collect();
    names.sort();
    let t = lua.create_table()?;
    for (i, n) in names.into_iter().enumerate() {
        t.set(i + 1, n)?;
    }
    Ok(t)
}

// ---------- Permission helper ----------

fn check_permission_inline(lua: &Lua, req: &Permission) -> mlua::Result<()> {
    let active = lua
        .app_data_ref::<ActiveContext>()
        .ok_or_else(|| mlua::Error::external("active context missing"))?;
    if active.effective_grants.contains(req) {
        Ok(())
    } else {
        Err(mlua::Error::external(inline_denial(
            &active,
            &format!("Lua tried to use host permission `{}`", req.as_str()),
            &[req.as_str().to_string()],
        )))
    }
}

fn inline_denial(active: &ActiveContext, what: &str, missing: &[String]) -> String {
    let location = if active.call_chain.is_empty() {
        "unknown Lua execution".to_string()
    } else {
        format!("call chain `{}`", active.call_chain.join(" -> "))
    };
    let caller = lua_caller_summary(&active.caller);
    let missing_text = missing
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "permission denied in {location}\nwhat: {what}\nmissing: {missing_text}\ncaller: {caller}\nfix: {}",
        inline_fix(active, missing)
    )
}

fn inline_fix(active: &ActiveContext, missing: &[String]) -> String {
    match (active.grant_kind.as_deref(), active.grant_name.as_deref()) {
        (Some(kind @ ("tool" | "service")), Some(name)) => format!(
            "add to grants.toml:\n{}\ngranted = [{}]",
            toml_table(kind, name),
            toml_array(missing)
        ),
        _ => format!(
            "add the missing grant to the tool or service that owns this execution: [{}]",
            toml_array(missing)
        ),
    }
}

fn lua_caller_summary(caller: &agentd_permissions::Caller) -> String {
    let mut parts = Vec::new();
    if let Some(v) = &caller.runner {
        parts.push(format!("runner `{}`", v.as_str()));
    }
    if let Some(v) = &caller.interface {
        parts.push(format!("interface `{}`", v.as_str()));
    }
    if let Some(v) = &caller.service {
        parts.push(format!("service `{}`", v.as_str()));
    }
    if let Some(v) = &caller.session {
        parts.push(format!("session `{}`", v.as_str()));
    }
    if let Some(v) = &caller.user {
        parts.push(format!("user `{}`", v.as_str()));
    }
    if parts.is_empty() {
        "direct call".to_string()
    } else {
        parts.join(", ")
    }
}

fn toml_table(section: &str, name: &str) -> String {
    format!("[{section}.{}]", toml_key(name))
}

fn toml_key(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        toml_string(name)
    }
}

fn toml_array(items: &[String]) -> String {
    items
        .iter()
        .map(|s| toml_string(s))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---------- Registry impl ----------

#[async_trait]
impl Registry for LuaHost {
    fn list(&self) -> Vec<String> {
        let cat = self.catalog.read().unwrap();
        let mut v: Vec<String> = cat.actions.keys().cloned().collect();
        v.sort();
        v
    }

    fn action_info(&self, name: &str) -> Option<RegistryActionInfo> {
        let cat = self.catalog.read().unwrap();
        if !cat.actions.contains_key(name) {
            return None;
        }
        let meta = cat.action_meta.get(name).cloned().unwrap_or_default();
        Some(RegistryActionInfo {
            name: name.to_string(),
            tool: meta.tool,
            requires: meta.requires,
            confirm: meta.confirm,
        })
    }

    fn tool_info(&self, name: &str) -> Option<RegistryToolInfo> {
        let cat = self.catalog.read().unwrap();
        cat.tools.get(name).map(|t| RegistryToolInfo {
            name: name.to_string(),
            requires: t.requires.clone(),
        })
    }

    async fn call(
        &self,
        ctx: CallContext,
        call: ActionCall,
    ) -> Result<ActionResult, RegistryError> {
        let lua = self.lua.clone();
        let catalog = self.catalog.clone();
        let ActionCall { action, args } = call;
        let effective_grants = ctx.effective_grants.clone();
        let call_chain = ctx.call_chain.clone();

        // Phase 1: create coroutine for the handler. ActiveContext is now
        // bound per-resume by the scheduler (not here) so concurrent calls
        // / services don't stomp each other's grants.
        let setup = {
            let lua = lua.clone();
            let catalog = catalog.clone();
            let action = action.clone();
            tokio::task::spawn_blocking(
                move || -> Result<(mlua::Thread, Option<String>), RegistryError> {
                    let lua_g = lua.lock().unwrap();
                    let cat = catalog.read().unwrap();
                    let key = cat
                        .actions
                        .get(&action)
                        .ok_or_else(|| RegistryError::NotFound(action.clone()))?;
                    let grant_name = cat.action_meta.get(&action).and_then(|m| m.tool.clone());
                    let func: Function = lua_g
                        .registry_value(key)
                        .map_err(|e| RegistryError::Invocation(e.to_string()))?;
                    let thread = ctx_thread(&lua_g, func, true)
                        .map_err(|e| RegistryError::Invocation(e.to_string()))?;
                    Ok((thread, grant_name))
                },
            )
            .await
            .map_err(|e| RegistryError::Invocation(format!("join: {e}")))?
        };
        let (thread, grant_name) = setup?;
        let ctx_for_drive = ActiveContext {
            caller: ctx.caller.clone(),
            effective_grants,
            call_chain,
            grant_kind: grant_name.as_ref().map(|_| "tool".to_string()),
            grant_name,
        };

        // Phase 2: drive. Scheduler swaps ActiveContext into app-data on
        // each resume and restores default on the way out.
        let outcome = scheduler::drive(lua.clone(), thread, vec![args], ctx_for_drive).await;

        outcome
            .map(|value| ActionResult { value })
            .map_err(|e| RegistryError::Invocation(e.to_string()))
    }

    fn list_services(&self) -> Vec<String> {
        let cat = self.catalog.read().unwrap();
        let mut v: Vec<String> = cat.services.keys().cloned().collect();
        v.sort();
        v
    }

    async fn call_service(&self, ctx: CallContext, name: &str) -> Result<(), RegistryError> {
        let lua = self.lua.clone();
        let catalog = self.catalog.clone();
        let svc_name = name.to_string();
        let ctx_for_drive = ActiveContext {
            caller: ctx.caller.clone(),
            effective_grants: ctx.effective_grants.clone(),
            call_chain: ctx.call_chain.clone(),
            grant_kind: Some("service".to_string()),
            grant_name: Some(svc_name.clone()),
        };

        let thread = {
            let lua = lua.clone();
            let catalog = catalog.clone();
            let svc_name = svc_name.clone();
            tokio::task::spawn_blocking(move || -> Result<mlua::Thread, RegistryError> {
                let lua_g = lua.lock().unwrap();
                let cat = catalog.read().unwrap();
                let key = cat
                    .services
                    .get(&svc_name)
                    .ok_or_else(|| RegistryError::NotFound(format!("service `{svc_name}`")))?;
                let func: Function = lua_g
                    .registry_value(key)
                    .map_err(|e| RegistryError::Invocation(e.to_string()))?;
                let thread = ctx_thread(&lua_g, func, false)
                    .map_err(|e| RegistryError::Invocation(e.to_string()))?;
                Ok(thread)
            })
            .await
            .map_err(|e| RegistryError::Invocation(format!("join: {e}")))??
        };

        let outcome = scheduler::drive(lua.clone(), thread, vec![], ctx_for_drive).await;

        outcome
            .map(|_| ())
            .map_err(|e| RegistryError::Invocation(e.to_string()))
    }
}

#[cfg(test)]
mod sandbox_policy_tests {
    use super::build_sandbox_policy;
    use agentd_permissions::{Permission, PermissionSet};
    use std::path::PathBuf;

    #[test]
    fn maps_fs_and_net_grants() {
        let mut grants = PermissionSet::empty();
        grants.insert(Permission::new("fs.write:/allowed/**"));
        grants.insert(Permission::new("fs.read:/data/*"));
        grants.insert(Permission::new("net:api.example.com"));
        let p = build_sandbox_policy(&grants);
        assert!(p.write_paths.contains(&PathBuf::from("/allowed")));
        assert!(p.read_paths.contains(&PathBuf::from("/data")));
        assert!(p.allow_net);
        assert!(!p.unrestricted);
    }

    #[test]
    fn detects_unrestricted() {
        let mut grants = PermissionSet::empty();
        grants.insert(Permission::new("shell.unrestricted"));
        let p = build_sandbox_policy(&grants);
        assert!(p.unrestricted);
    }

    #[test]
    fn collects_net_hosts() {
        let mut grants = PermissionSet::empty();
        grants.insert(Permission::new("net:api.example.com"));
        grants.insert(Permission::new("net:*"));
        let p = build_sandbox_policy(&grants);
        assert!(p.allow_net);
        assert_eq!(p.net_hosts.len(), 2);
    }

    #[test]
    fn ignores_relative_and_non_fs_slugs() {
        let mut grants = PermissionSet::empty();
        grants.insert(Permission::new("fs.write:relative/path"));
        grants.insert(Permission::new("shell.exec:git"));
        let p = build_sandbox_policy(&grants);
        assert!(p.write_paths.is_empty());
        assert!(!p.allow_net);
        assert!(!p.unrestricted);
    }
}

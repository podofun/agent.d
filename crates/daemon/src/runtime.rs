//! Runtime assembly. [`build_runtime`] turns the configured `init.lua` plus
//! `grants.toml` into a live [`Executor`], its Lua [`LuaHost`], the spawned
//! service tasks, and the set of files the runtime actually loaded.
//!
//! The daemon calls it once at startup and again on every `--watch` hot reload.
//! The pieces that must survive a reload (providers, keyring, durable memory,
//! the trace sink, the approval broker, the async runtime handle) live in
//! [`Shared`] and are built once in `main::run`; everything else is rebuilt
//! fresh so a reload leaves no stale registrations behind.

use std::path::PathBuf;
use std::sync::Arc;

use agentd_ai::ProviderRegistry;
use agentd_executor::{Executor, ExecutorHandle};
use agentd_memory::RedbStore;
use agentd_permissions::{Engine, Grants, GrantsFile, load_grants_file};
use agentd_scripting::LuaHost;
use agentd_secrets::KeyringStore;
use agentd_trace::JsonlSink;
use agentd_types::Registry;
use anyhow::{Result, anyhow, bail};
use tokio::task::JoinHandle;

use crate::config::Config;

/// Long-lived dependencies reused across every runtime build. Constructed once
/// at daemon startup; cloned into each [`build_runtime`] call.
pub struct Shared {
    pub providers: Arc<ProviderRegistry>,
    pub keyring: Arc<KeyringStore>,
    pub memory: Arc<RedbStore>,
    pub trace: Arc<JsonlSink>,
    pub broker: Arc<agentd_approvals::Broker>,
    pub async_handle: tokio::runtime::Handle,
    pub packages_root: PathBuf,
}

/// One fully-assembled runtime. Dropping it (after breaking the host's internal
/// cycles via [`teardown`]) releases the Lua VM and all registrations.
pub struct BuiltRuntime {
    pub executor: Arc<Executor>,
    pub host: Arc<LuaHost>,
    pub service_handles: Vec<JoinHandle<()>>,
    /// Every file the runtime loaded: `init.lua`, its `import()` targets, loaded
    /// skill sources, and `grants.toml`. The watch loop derives its file set
    /// from this and re-derives it after each reload.
    pub used_paths: Vec<PathBuf>,
}

/// Build a complete runtime from `cfg` and the shared dependencies. Evaluates
/// `init.lua`, expands package grants, constructs the executor, wires the runner
/// dispatcher, and spawns services.
pub fn build_runtime(cfg: &Config, shared: &Shared) -> Result<BuiltRuntime> {
    let host = Arc::new(LuaHost::new()?);
    // `import("foo.lua")` resolves relative to init.lua's parent.
    if let Some(root) = cfg.init_file.parent() {
        host.set_root(root);
    }
    // The workspace root is the default cwd for relative `fs.*` paths and the
    // anchor for relative grant specs.
    host.set_workspace_root(&cfg.workspace_root);
    host.set_packages_root(&shared.packages_root);
    host.start_async_runtime(shared.async_handle.clone());
    host.set_secrets(shared.keyring.clone());
    host.set_memory(shared.memory.clone());
    for name in shared.providers.names() {
        if let Some(p) = shared.providers.get(&name) {
            host.set_ai_provider(name, p);
        }
    }
    if let Some(d) = shared.providers.default_name() {
        host.set_default_ai_provider(d);
    }

    if !cfg.init_file.exists() {
        bail!(
            "no init.lua at {}. Point --init / runtime.init at your entry file.",
            cfg.init_file.display()
        );
    }
    host.load_file(&cfg.init_file)?;

    let skills = host.skills();
    let runners = host.runners();
    let services = host.services();

    let mut grants_file = load_grants_file(&cfg.grants_file).map_err(|e| anyhow!(e))?;
    let loaded_packages = host.loaded_packages();
    agentd_packages::expand_grants(&loaded_packages, &mut grants_file);
    warn_orphan_tool_grants(host.as_ref(), &grants_file);
    let engine = Arc::new(Engine::new(Grants::from_file(grants_file)));

    let registry: Arc<dyn Registry> = host.clone();
    let mut executor = Executor::new(
        registry,
        shared.trace.clone(),
        engine,
        runners,
        services,
        skills,
        shared.providers.clone(),
    );
    executor.set_max_runner_turns(cfg.max_turns);

    // "allow forever" reload pipeline: rebuild the engine from grants.toml,
    // re-applying this build's package desugaring.
    {
        let reload_host = host.clone();
        let reload_path = cfg.grants_file.clone();
        let reload = Arc::new(move || -> std::result::Result<Engine, String> {
            let mut gf = load_grants_file(&reload_path).map_err(|e| e.to_string())?;
            let pkgs = reload_host.loaded_packages();
            agentd_packages::expand_grants(&pkgs, &mut gf);
            Ok(Engine::new(Grants::from_file(gf)))
        });
        executor.set_broker(shared.broker.clone());
        executor.set_grants_path(cfg.grants_file.clone());
        executor.set_reload_grants(reload);
    }

    let executor = Arc::new(executor);
    // Wire the executor back into Lua so `ctx.run` / `agentd.runners.run` work.
    host.set_runner_dispatcher(ExecutorHandle::new(executor.clone()));

    let service_handles = executor.start_services();
    let used_paths = used_paths(&host, &cfg.grants_file);

    Ok(BuiltRuntime {
        executor,
        host,
        service_handles,
        used_paths,
    })
}

/// Warn for every `[tool.<name>]` in grants.toml that has no matching tool
/// registered in Lua. Such a grant silently grants nothing — the engine binds
/// grants to the tool that owns the action (by name), so a typo or a name that
/// doesn't match any `agentd.tool{...}` is a common, hard-to-debug footgun.
fn warn_orphan_tool_grants(reg: &dyn Registry, grants: &GrantsFile) {
    let actions = reg.list();
    for name in grants.tool.keys() {
        // The engine binds `[tool.<name>]` grants to the registered tool OR
        // to any action's `<name>.` namespace (engine falls back to
        // `action.tool`), so a bare-action namespace is a live grant, not an
        // orphan.
        let covers_namespace = actions.iter().any(|a| {
            a.strip_prefix(name.as_str())
                .is_some_and(|r| r.starts_with('.'))
        });
        if reg.tool_info(name).is_none() && !covers_namespace {
            tracing::warn!(
                tool = %name,
                "grants.toml declares `[tool.{name}]` but no tool or action namespace `{name}` is registered in Lua; this grant has no effect"
            );
        }
    }
}

/// The file set a runtime loaded: init + `import()`ed Lua, skill sources, grants.
fn used_paths(host: &LuaHost, grants_file: &std::path::Path) -> Vec<PathBuf> {
    let mut set: Vec<PathBuf> = host.imported_paths();
    set.extend(host.skills().sources());
    set.push(canonicalize_or(grants_file));
    set.sort();
    set.dedup();
    set
}

fn canonicalize_or(p: &std::path::Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Break the old runtime's internal reference cycles so it can drop on reload:
/// the host parks an `Arc<Executor>` (runner dispatcher) and the async driver
/// task keeps the Lua VM alive via a sender stored in the VM. Also abort the
/// old service tasks (they loop forever otherwise).
pub fn teardown(built: BuiltRuntime) {
    for h in &built.service_handles {
        h.abort();
    }
    built.host.clear_runner_dispatcher();
    built.host.shutdown_async_runtime();
    // `built` drops here; in-flight callers holding `Arc<Executor>` clones
    // finish on the old runtime, then the host and Lua VM drop.
}

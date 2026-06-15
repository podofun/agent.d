use agentd_ai::{
    ClaudeApiProvider, ClaudeCliProvider, CodexAppServerProvider, CodexCliProvider,
    OpenAiApiProvider, ProviderRegistry,
};
use agentd_api::{AppState, router, serve};
use agentd_executor::{Executor, ExecutorHandle};
use agentd_memory::RedbStore;
use agentd_permissions::{Engine, Grants, load_grants_file};
use agentd_scripting::LuaHost;
use agentd_secrets::KeyringStore;
use agentd_trace::JsonlSink;
use agentd_types::Registry;
use anyhow::{Result, anyhow, bail};
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

mod config;
use config::{Cli, Config};

fn main() -> Result<()> {
    // If this process was re-exec'd as the in-netns network supervisor, run it
    // and exit here — BEFORE any threads or the async runtime start (the
    // supervisor forks the sandboxed command and must be single-threaded).
    agentd_shell::sandbox::run_netns_supervisor_if_requested();
    run()
}

#[tokio::main]
async fn run() -> Result<()> {
    let cfg = Config::resolve(Cli::parse())?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log_level))
        .init();
    if cfg.yolo {
        tracing::warn!(
            "runtime.yolo is reserved and currently ignored; the permission engine remains enforced"
        );
    }
    tracing::info!(?cfg, "starting agentd");

    let keyring = Arc::new(KeyringStore::default_service());
    let mut providers = ProviderRegistry::new();
    let anthropic_api: Arc<dyn agentd_ai::Provider> =
        Arc::new(ClaudeApiProvider::new(keyring.clone()));
    let anthropic_cli: Arc<dyn agentd_ai::Provider> = Arc::new(ClaudeCliProvider::new());
    let openai_api: Arc<dyn agentd_ai::Provider> =
        Arc::new(OpenAiApiProvider::new(keyring.clone()));
    let codex: Arc<dyn agentd_ai::Provider> = Arc::new(CodexAppServerProvider::new());
    let openai_cli: Arc<dyn agentd_ai::Provider> = Arc::new(CodexCliProvider::new());
    providers.insert("anthropic", anthropic_api);
    providers.insert("anthropic-cli", anthropic_cli);
    providers.insert("openai", openai_api);
    providers.insert("codex", codex);
    providers.insert("openai-cli", openai_cli);
    providers.set_default("anthropic");
    let providers = Arc::new(providers);

    let host = Arc::new(LuaHost::new()?);
    // `agentd.import("foo.lua")` resolves relative to init.lua's parent.
    if let Some(root) = cfg.init_file.parent() {
        host.set_root(root);
    }
    // `agentd.import("name")` (bare) resolves installed packages here.
    let packages_root = dirs::data_dir()
        .map(|d| d.join("agentd").join("packages"))
        .ok_or_else(|| anyhow!("no XDG data dir for packages"))?;
    host.set_packages_root(&packages_root);
    // Boot the background coroutine driver before init.lua so any `async(fn)`
    // call from init / tools / services has a runtime to spawn onto.
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.set_secrets(keyring.clone());
    // Durable `ctx.memory` store — one redb file under the XDG data dir.
    let memory_path = dirs::data_dir()
        .map(|d| d.join("agentd").join("memory.redb"))
        .ok_or_else(|| anyhow!("no XDG data dir for memory"))?;
    if let Some(parent) = memory_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let memory = RedbStore::open(&memory_path)
        .map_err(|e| anyhow!("open memory db {}: {e}", memory_path.display()))?;
    host.set_memory(Arc::new(memory));
    for name in providers.names() {
        if let Some(p) = providers.get(&name) {
            host.set_ai_provider(name, p);
        }
    }
    if let Some(d) = providers.default_name() {
        host.set_default_ai_provider(d);
    }

    if !cfg.init_file.exists() {
        bail!(
            "no init.lua at {}. Point --init / runtime.init at your entry file.",
            cfg.init_file.display()
        );
    }
    host.load_file(&cfg.init_file)?;
    tracing::info!(
        init = %cfg.init_file.display(),
        actions = host.list().len(),
        runners = host.runners().len(),
        services = host.services().len(),
        skills = host.skills().len(),
        "init.lua evaluated"
    );

    let skills = host.skills();
    let runners = host.runners();
    let services = host.services();

    let trace = JsonlSink::open(&cfg.trace_file).await?;
    tracing::info!(trace = %trace.path().display(), "trace open");

    let mut grants_file = load_grants_file(&cfg.grants_file).map_err(|e| anyhow!(e))?;
    // Desugar every trusted package into the per-tool/runner/service grant rows
    // the engine enforces. Untouched engine, default-deny preserved.
    let loaded_packages = host.loaded_packages();
    agentd_packages::expand_grants(&loaded_packages, &mut grants_file);
    tracing::info!(packages = loaded_packages.len(), "package grants expanded");
    tracing::info!(
        grants = %cfg.grants_file.display(),
        tools = grants_file.tool.len(),
        runners = grants_file.runner.len(),
        services = grants_file.service.len(),
        interfaces = grants_file.interface.len(),
        "grants loaded"
    );
    let engine = Arc::new(Engine::new(Grants::from_file(grants_file)));

    let registry: Arc<dyn Registry> = host.clone();
    let mut executor = Executor::new(
        registry,
        Arc::new(trace),
        engine,
        runners,
        services,
        skills,
        providers,
    );
    executor.set_max_runner_turns(cfg.max_turns);

    // Interactive approval: escalate Tool-missing / confirm denials to a
    // connected operator on the control plane. The reload closure reproduces
    // the daemon's grants-build pipeline so an "allow forever" verdict
    // re-applies package desugaring on hot-reload.
    let broker = Arc::new(agentd_approvals::Broker::new(
        std::time::Duration::from_millis(cfg.approval_timeout_ms),
    ));
    {
        let reload_host = host.clone();
        let reload_path = cfg.grants_file.clone();
        let reload = Arc::new(move || -> std::result::Result<Engine, String> {
            let mut gf = load_grants_file(&reload_path).map_err(|e| e.to_string())?;
            let pkgs = reload_host.loaded_packages();
            agentd_packages::expand_grants(&pkgs, &mut gf);
            Ok(Engine::new(Grants::from_file(gf)))
        });
        executor.set_broker(broker.clone());
        executor.set_grants_path(cfg.grants_file.clone());
        executor.set_reload_grants(reload);
    }

    let executor = Arc::new(executor);
    // Now that the executor exists, wire it back into Lua so
    // `agentd.runners.run(name, opts)` works from any script.
    host.set_runner_dispatcher(ExecutorHandle::new(executor.clone()));

    // Spawn services as background tasks. Each runs in its own Tokio task,
    // supervised by the executor's service registry. Handles dropped here =
    // tasks keep running until daemon exit.
    let handles = executor.start_services();
    tracing::info!(spawned = handles.len(), "services started");

    let auth_token = resolve_ws_token(&cfg)?;
    let admin_token = resolve_admin_token(&cfg)?;
    let state = AppState {
        executor: executor.clone(),
        auth_token: auth_token.map(Arc::new),
        admin_token: admin_token.map(Arc::new),
        broker,
    };

    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    tracing::info!(addr = %cfg.addr, "listening");
    serve(listener, router(state)).await?;
    Ok(())
}

/// Decide the effective `/ws` bearer token. `--no-auth` → `None`. Otherwise use
/// the configured token, or mint one and persist it to the token file (0600)
/// so a local `agentctl` picks it up automatically.
fn resolve_ws_token(cfg: &Config) -> Result<Option<String>> {
    if cfg.no_auth {
        tracing::warn!("--no-auth: /ws accepts any local connection without a token");
        return Ok(None);
    }
    if let Some(token) = &cfg.auth_token {
        return Ok(Some(token.clone()));
    }
    let token = gen_token();
    let path = config::default_token_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(token_file = %path.display(), "minted /ws auth token");
    Ok(Some(token))
}

/// Decide the effective `/control` admin token. `--no-auth` → `None`. Otherwise
/// use the configured admin token, or mint one and persist it to the
/// admin-token file (0600) so a local `agentctl grants listen` picks it up.
fn resolve_admin_token(cfg: &Config) -> Result<Option<String>> {
    if cfg.no_auth {
        tracing::warn!("--no-auth: /control accepts any local connection without a token");
        return Ok(None);
    }
    if let Some(token) = &cfg.admin_token {
        return Ok(Some(token.clone()));
    }
    let token = gen_token();
    let path = config::default_admin_token_file()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(admin_token_file = %path.display(), "minted /control admin token");
    Ok(Some(token))
}

/// 256 bits of OS randomness, hex-encoded.
fn gen_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS RNG unavailable");
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

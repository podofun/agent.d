use agentd_ai::{
    ClaudeApiProvider, ClaudeCliProvider, CodexAppServerProvider, CodexCliProvider,
    OpenAiApiProvider, Provider as AIProvider, ProviderRegistry,
};
use agentd_api::{AppState, router, serve};
use agentd_memory::RedbStore;
use agentd_secrets::KeyringStore;
use agentd_shell::sandbox;
use agentd_trace::JsonlSink;
use agentd_types::Registry;
use anyhow::{Result, anyhow};
use clap::Parser;
use std::io::IsTerminal;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;

mod config;
#[cfg(target_os = "windows")]
mod elevate;
mod runtime;
mod watch;
use config::{Cli, Config};
use runtime::{Shared, build_runtime};

fn main() -> Result<()> {
    // If this process was re-exec'd as the in-netns network supervisor, run it
    // and exit here — BEFORE any threads or the async runtime start (the
    // supervisor forks the sandboxed command and must be single-threaded).
    sandbox::run_netns_supervisor_if_requested();

    let cli = Cli::parse();
    // One-time sandbox setup runs synchronously and exits. The daemon itself
    // never elevates: it creates the (non-privileged) sandbox profiles, then
    // launches the separate broker binary through UAC to register the SYSTEM
    // service that performs privileged network filtering.
    if cli.install_sandbox {
        sandbox::install().map_err(|e| anyhow!("sandbox profile setup failed: {e}"))?;
        #[cfg(target_os = "windows")]
        {
            let broker = std::env::current_exe()?
                .parent()
                .ok_or_else(|| anyhow!("cannot locate the daemon's directory"))?
                .join("agentd-netbroker.exe");
            if !broker.exists() {
                return Err(anyhow!(
                    "agentd-netbroker.exe not found next to daemon.exe (looked in {})",
                    broker.display()
                ));
            }
            if elevate::run_elevated(&broker, "--install")? {
                println!("Network sandbox installed. The daemon runs without Administrator.");
            } else {
                return Err(anyhow!(
                    "Administrator approval was declined; nothing was changed."
                ));
            }
        }
        #[cfg(not(target_os = "windows"))]
        println!("Sandbox setup complete. The daemon runs unprivileged from here on.");
        return Ok(());
    }
    if cli.uninstall_sandbox {
        sandbox::uninstall().map_err(|e| anyhow!("sandbox teardown failed: {e}"))?;
        println!("Sandbox setup removed.");
        return Ok(());
    }
    run(cli)
}

#[tokio::main]
async fn run(cli: Cli) -> Result<()> {
    let started = Instant::now();
    let cfg = Config::resolve(cli)?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cfg.log_level))
        .compact()
        .with_target(false)
        .without_time()
        .init();
    if cfg.yolo {
        tracing::warn!(
            "runtime.yolo is reserved and currently ignored; the permission engine remains enforced"
        );
    }
    tracing::debug!(?cfg, "starting agentd");

    let keyring = Arc::new(KeyringStore::default_service());
    let mut providers = ProviderRegistry::new();
    let anthropic_api: Arc<dyn AIProvider> = Arc::new(ClaudeApiProvider::new(keyring.clone()));
    let anthropic_cli: Arc<dyn AIProvider> = Arc::new(ClaudeCliProvider::new());
    let openai_api: Arc<dyn AIProvider> = Arc::new(OpenAiApiProvider::new(keyring.clone()));
    let codex: Arc<dyn AIProvider> = Arc::new(CodexAppServerProvider::new());
    let openai_cli: Arc<dyn AIProvider> = Arc::new(CodexCliProvider::new());
    providers.insert("anthropic", anthropic_api);
    providers.insert("anthropic-cli", anthropic_cli);
    providers.insert("openai", openai_api);
    providers.insert("codex", codex);
    providers.insert("openai-cli", openai_cli);
    register_configured_providers(&mut providers, &cfg, keyring.clone());
    let providers = Arc::new(providers);

    // `agentd.import("name")` (bare) resolves installed packages here.
    let packages_root = dirs::data_dir()
        .map(|d| d.join("agentd").join("packages"))
        .ok_or_else(|| anyhow!("no XDG data dir for packages"))?;
    // Durable `ctx.memory` store — one redb file under the XDG data dir, shared
    // across hot reloads so memory survives.
    let memory_path = dirs::data_dir()
        .map(|d| d.join("agentd").join("memory.redb"))
        .ok_or_else(|| anyhow!("no XDG data dir for memory"))?;
    if let Some(parent) = memory_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let memory = RedbStore::open(&memory_path)
        .map_err(|e| anyhow!("open memory db {}: {e}", memory_path.display()))?;

    let trace = JsonlSink::open(&cfg.trace_file).await?;
    tracing::debug!(trace = %trace.path().display(), "trace open");

    // Interactive approval broker, shared across reloads so a connected operator
    // stays subscribed through a hot reload.
    let broker = Arc::new(agentd_approvals::Broker::new(
        std::time::Duration::from_millis(cfg.approval_timeout_ms),
    ));

    // Dependencies that must outlive any single runtime build.
    let shared = Shared {
        providers,
        keyring,
        memory: Arc::new(memory),
        trace: Arc::new(trace),
        broker: broker.clone(),
        async_handle: tokio::runtime::Handle::current(),
        packages_root,
    };

    let built = build_runtime(&cfg, &shared)?;
    tracing::debug!(
        init = %cfg.init_file.display(),
        actions = built.host.list().len(),
        runners = built.host.runners().len(),
        services = built.host.services().len(),
        skills = built.host.skills().len(),
        services_spawned = built.service_handles.len(),
        "init.lua evaluated"
    );

    // Hot-swappable executor: the watch loop `store()`s a fresh one on reload.
    let executor = Arc::new(arc_swap::ArcSwap::from(built.executor.clone()));

    // Counts for the startup banner, captured before `built` is moved into the
    // watcher below.
    let (n_actions, n_runners, n_services, n_skills) = (
        built.host.list().len(),
        built.host.runners().len(),
        built.host.services().len(),
        built.host.skills().len(),
    );

    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    let local_addr = listener.local_addr()?;
    let startup = StartupSummary {
        bind_addr: local_addr,
        init_file: &cfg.init_file,
        actions: n_actions,
        runners: n_runners,
        services: n_services,
        skills: n_skills,
        elapsed: started.elapsed(),
        color: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
    };

    // Start the dev hot-reload watcher (or let `built` keep its services running
    // detached). The watcher takes ownership of `built` and `shared` so it can
    // rebuild and tear down old runtimes on file changes.
    if cfg.watch {
        watch::spawn(cfg.clone(), shared, executor.clone(), built);
    }

    let auth_token = resolve_ws_token(&cfg)?;
    let admin_token = resolve_admin_token(&cfg)?;
    let state = AppState {
        executor,
        auth_token: auth_token.map(Arc::new),
        admin_token: admin_token.map(Arc::new),
        broker,
    };

    println!("{}", startup.render());
    tracing::debug!(addr = %local_addr, "listening");
    serve(listener, router(state)).await?;
    Ok(())
}

/// Register user-configured `[providers.<name>]` entries on top of the
/// built-ins and apply `runtime.default_provider`. Config validation
/// (reserved names, auth choice) already happened in `Config::resolve`.
fn register_configured_providers(
    providers: &mut ProviderRegistry,
    cfg: &Config,
    keyring: Arc<dyn agentd_secrets::SecretStore>,
) {
    use config::ProviderKind;
    for (name, spec) in &cfg.providers {
        let p: Arc<dyn AIProvider> = match spec.kind {
            ProviderKind::OpenAi => {
                let mut prov = OpenAiApiProvider::new(keyring.clone())
                    .with_name(name.clone())
                    .with_endpoint(&spec.base_url);
                prov = match (&spec.api_key_secret, spec.auth.as_deref()) {
                    (Some(k), _) => prov.with_secret_key(k.clone()),
                    (None, _) => prov.with_no_auth(),
                };
                if let Some(m) = &spec.default_model {
                    prov = prov.with_default_model(m.clone());
                }
                Arc::new(prov)
            }
            ProviderKind::Anthropic => {
                let mut prov = ClaudeApiProvider::new(keyring.clone())
                    .with_name(name.clone())
                    .with_endpoint(&spec.base_url);
                prov = match (&spec.api_key_secret, spec.auth.as_deref()) {
                    (Some(k), _) => prov.with_secret_key(k.clone()),
                    (None, _) => prov.with_no_auth(),
                };
                if let Some(m) = &spec.default_model {
                    prov = prov.with_default_model(m.clone());
                }
                Arc::new(prov)
            }
        };
        providers.insert(name.clone(), p);
    }
    providers.set_default(&cfg.default_provider);
}

struct StartupSummary<'a> {
    bind_addr: SocketAddr,
    init_file: &'a std::path::Path,
    actions: usize,
    runners: usize,
    services: usize,
    skills: usize,
    elapsed: Duration,
    color: bool,
}

impl StartupSummary<'_> {
    fn render(&self) -> String {
        let brand = if self.color {
            "\x1b[1;35mAGENTD\x1b[0m"
        } else {
            "AGENTD"
        };
        let accent = if self.color { "\x1b[36m" } else { "" };
        let reset = if self.color { "\x1b[0m" } else { "" };
        let base = url_host(self.bind_addr);
        let port = self.bind_addr.port();

        format!(
            "\n  {brand} v{}  ready in {} ms\n\n  Local:   {accent}http://{base}:{port}/{reset}\n  WS:      {accent}ws://{base}:{port}/ws{reset}\n  Control: {accent}ws://{base}:{port}/control{reset}\n  Loaded:  {}, {}, {}, {}\n  Init:    {}\n  Logs:    warnings/errors (AGENTD_LOG=debug for detail)\n",
            env!("CARGO_PKG_VERSION"),
            self.elapsed.as_millis(),
            count_label(self.actions, "action"),
            count_label(self.runners, "runner"),
            count_label(self.services, "service"),
            count_label(self.skills, "skill"),
            self.init_file.display(),
        )
    }
}

fn url_host(addr: SocketAddr) -> String {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => "localhost".to_string(),
        IpAddr::V6(ip) if ip.is_unspecified() => "localhost".to_string(),
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    }
}

fn count_label(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
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
    tracing::debug!(token_file = %path.display(), "minted /ws auth token");
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
    tracing::debug!(admin_token_file = %path.display(), "minted /control admin token");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    fn cfg_with(body: &str) -> Config {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("config.toml");
        std::fs::write(&p, body).unwrap();
        Config::resolve(Cli {
            config: Some(p),
            ..Cli::default()
        })
        .unwrap()
    }

    #[test]
    fn configured_openai_provider_lands_in_registry() {
        let cfg = cfg_with(
            r#"
            [providers.openrouter]
            kind = "openai"
            base_url = "https://openrouter.ai/api/v1"
            api_key_secret = "openrouter_api_key"

            [providers.ollama]
            kind = "openai"
            base_url = "http://localhost:11434/v1"
            auth = "none"
        "#,
        );
        let keyring = Arc::new(agentd_secrets::MemoryStore::default());
        let mut providers = ProviderRegistry::new();
        register_configured_providers(&mut providers, &cfg, keyring);
        assert!(providers.get("openrouter").is_some());
        assert!(providers.get("ollama").is_some());
        let (name, _, model) = providers
            .resolve_for_model("openrouter/meta-llama/llama-3.3-70b")
            .unwrap();
        assert_eq!(name, "openrouter");
        assert_eq!(model, "meta-llama/llama-3.3-70b");
    }

    #[test]
    fn default_provider_from_config_is_applied() {
        let cfg = cfg_with(
            r#"
            [runtime]
            default_provider = "ollama"

            [providers.ollama]
            kind = "openai"
            base_url = "http://localhost:11434/v1"
            auth = "none"
        "#,
        );
        let keyring = Arc::new(agentd_secrets::MemoryStore::default());
        let mut providers = ProviderRegistry::new();
        register_configured_providers(&mut providers, &cfg, keyring);
        assert_eq!(providers.default_name(), Some("ollama"));
    }

    #[test]
    fn startup_summary_is_compact_and_actionable() {
        let rendered = StartupSummary {
            bind_addr: "127.0.0.1:7777".parse().unwrap(),
            init_file: Path::new("/tmp/init.lua"),
            actions: 2,
            runners: 1,
            services: 0,
            skills: 3,
            elapsed: Duration::from_millis(159),
            color: false,
        }
        .render();

        assert!(rendered.contains("AGENTD v"));
        assert!(rendered.contains("ready in 159 ms"));
        assert!(rendered.contains("Local:   http://127.0.0.1:7777/"));
        assert!(rendered.contains("WS:      ws://127.0.0.1:7777/ws"));
        assert!(rendered.contains("Loaded:  2 actions, 1 runner, 0 services, 3 skills"));
        assert!(rendered.contains("Logs:    warnings/errors"));
        assert_eq!(rendered.lines().count(), 9);
    }

    #[test]
    fn startup_summary_uses_localhost_for_unspecified_binds() {
        let rendered = StartupSummary {
            bind_addr: "0.0.0.0:7777".parse().unwrap(),
            init_file: Path::new("/tmp/init.lua"),
            actions: 0,
            runners: 0,
            services: 0,
            skills: 0,
            elapsed: Duration::from_millis(1),
            color: false,
        }
        .render();

        assert!(rendered.contains("http://localhost:7777/"));
    }
}

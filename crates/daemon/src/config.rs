use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use serde::Deserialize;

/// CLI args. clap reads `env = "..."` automatically, so env var fallback
/// is folded into this struct before [`Config::resolve`] sees it.
#[derive(Parser, Debug, Clone, Default)]
#[command(name = "daemon", about = "agentd runtime")]
pub struct Cli {
    /// Path to `config.toml`. Defaults to `$XDG_CONFIG_HOME/agentd/config.toml`.
    #[arg(long, env = "AGENTD_CONFIG")]
    pub config: Option<PathBuf>,

    /// Path to `init.lua` (Lua userland entry). Overrides `runtime.init`.
    #[arg(long, env = "AGENTD_INIT")]
    pub init: Option<PathBuf>,

    /// WebSocket + HTTP /health bind address. Overrides `daemon.addr`.
    #[arg(long, env = "AGENTD_ADDR")]
    pub addr: Option<String>,

    /// JSONL trace sink path. Overrides `daemon.trace_file`.
    #[arg(long, env = "AGENTD_TRACE_FILE")]
    pub trace_file: Option<PathBuf>,

    /// tracing-subscriber filter. Overrides `daemon.log_level`.
    #[arg(long, env = "AGENTD_LOG")]
    pub log: Option<String>,

    /// Path to `grants.toml`. Defaults to `$XDG_CONFIG_HOME/agentd/grants.toml`.
    #[arg(long, env = "AGENTD_GRANTS_FILE")]
    pub grants_file: Option<PathBuf>,

    /// Bearer token clients must present on the `/ws` handshake. If unset and
    /// auth is enabled, the daemon generates one at startup. Overrides `daemon.token`.
    #[arg(long, env = "AGENTD_TOKEN")]
    pub token: Option<String>,

    /// Disable `/ws` authentication entirely (any local client may connect).
    #[arg(long, env = "AGENTD_NO_AUTH")]
    pub no_auth: bool,

    /// Bearer token clients must present on the privileged `/control`
    /// handshake. If unset and auth is enabled, the daemon generates one at
    /// startup. Overrides `daemon.admin_token`.
    #[arg(long, env = "AGENTD_ADMIN_TOKEN")]
    pub admin_token: Option<String>,

    /// How long (ms) an escalated permission request waits for an operator
    /// verdict before failing closed. Overrides `daemon.approval_timeout_ms`.
    #[arg(long, env = "AGENTD_APPROVAL_TIMEOUT_MS")]
    pub approval_timeout_ms: Option<u64>,

    /// Dev hot reload: watch init.lua plus the files it imports, loaded skill
    /// sources, and grants.toml, and rebuild the runtime in place on change.
    #[arg(long, env = "AGENTD_WATCH")]
    pub watch: bool,
}

/// Raw `config.toml` shape. All fields optional; missing == fall through
/// to the next source in the precedence chain.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub daemon: Option<RawDaemon>,
    #[serde(default)]
    pub runtime: Option<RawRuntime>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDaemon {
    #[serde(default)]
    pub addr: Option<String>,
    #[serde(default)]
    pub trace_file: Option<String>,
    #[serde(default)]
    pub log_level: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub no_auth: Option<bool>,
    #[serde(default)]
    pub admin_token: Option<String>,
    #[serde(default)]
    pub approval_timeout_ms: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRuntime {
    #[serde(default)]
    pub init: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub yolo: Option<bool>,
    #[serde(default)]
    pub watch: Option<bool>,
}

/// Final, fully-resolved daemon config.
#[derive(Debug, Clone)]
pub struct Config {
    /// Lua entry point. Sandboxed `agentd.import` resolves against its parent.
    pub init_file: PathBuf,
    pub trace_file: PathBuf,
    pub grants_file: PathBuf,
    pub addr: String,
    pub log_level: String,
    pub max_turns: u32,
    /// `/ws` auth is off when true — any local client may connect.
    pub no_auth: bool,
    /// Explicitly configured bearer token, if any. When auth is on and this is
    /// `None`, the daemon mints one at startup (see `default_token_file`).
    pub auth_token: Option<String>,
    /// Explicitly configured `/control` admin token. When auth is on and this
    /// is `None`, the daemon mints one at startup (see `default_admin_token_file`).
    pub admin_token: Option<String>,
    /// Approval wait budget (ms) before an escalated request fails closed.
    pub approval_timeout_ms: u64,
    /// Reserved. Setting true emits a warning on startup and otherwise has
    /// no effect today. Future implementation will bypass the 5-layer
    /// permission engine.
    pub yolo: bool,
    /// Dev hot reload: watch the used file set and rebuild the runtime in place.
    pub watch: bool,
}

impl Config {
    /// Resolve a `Config` from CLI args + env + `config.toml` + defaults.
    ///
    /// Per knob precedence: CLI > env (folded into `cli` via clap) >
    /// `config.toml` > built-in default. `log_level` has one extra
    /// fallback below config.toml: `RUST_LOG`, so dev workflows that
    /// already set it keep working.
    pub fn resolve(cli: Cli) -> Result<Self> {
        // 1. Find config.toml.
        let cfg_path = match cli.config {
            Some(p) => p,
            None => default_config_path()?,
        };

        // 2. Parse (missing file = empty; malformed = hard error).
        let raw: RawConfig = match std::fs::read_to_string(&cfg_path) {
            Ok(s) => toml::from_str(&s)
                .with_context(|| format!("malformed config.toml at {}", cfg_path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RawConfig::default(),
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("reading config.toml at {}", cfg_path.display())));
            }
        };
        let daemon = raw.daemon.unwrap_or_default();
        let runtime = raw.runtime.unwrap_or_default();

        // 3. Layered resolution.
        let addr = cli
            .addr
            .or(daemon.addr)
            .unwrap_or_else(|| "127.0.0.1:7777".to_string());

        let trace_file = cli
            .trace_file
            .or_else(|| daemon.trace_file.as_deref().map(expand_tilde))
            .unwrap_or_else(|| default_trace_file().expect("trace default"));

        let log_level = cli
            .log
            .or(daemon.log_level)
            .or_else(|| std::env::var("RUST_LOG").ok())
            .unwrap_or_else(|| "warn".to_string());

        let init_file = cli
            .init
            .or_else(|| runtime.init.as_deref().map(expand_tilde))
            .unwrap_or_else(|| default_init_file().expect("init default"));

        let grants_file = cli
            .grants_file
            .unwrap_or_else(|| default_grants_file().expect("grants default"));

        let max_turns = runtime.max_turns.unwrap_or(16);
        let yolo = runtime.yolo.unwrap_or(false);
        // `--watch` (flag/env) wins; otherwise fall back to config.toml.
        let watch = cli.watch || runtime.watch.unwrap_or(false);

        // `--no-auth` (flag/env) wins; otherwise fall back to config.toml.
        let no_auth = cli.no_auth || daemon.no_auth.unwrap_or(false);
        let auth_token = cli.token.or(daemon.token);
        let admin_token = cli.admin_token.or(daemon.admin_token);
        let approval_timeout_ms = cli
            .approval_timeout_ms
            .or(daemon.approval_timeout_ms)
            .unwrap_or(120_000);

        // Note on `yolo`: callers should emit the reserved-key warning
        // AFTER initializing the tracing subscriber. `Config::resolve` is
        // typically called before the subscriber exists; a warn here
        // would be silently dropped.

        Ok(Self {
            init_file,
            trace_file,
            grants_file,
            addr,
            log_level,
            max_turns,
            yolo,
            no_auth,
            auth_token,
            admin_token,
            approval_timeout_ms,
            watch,
        })
    }
}

/// Where the daemon persists an auto-generated `/ws` token so a local
/// `agentctl` can read it without the operator copying it by hand.
pub fn default_token_file() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("no XDG state/data dir")?;
    Ok(base.join("agentd").join("token"))
}

/// Where the daemon persists an auto-generated `/control` admin token so a
/// local `agentctl grants listen` can read it without manual copying.
pub fn default_admin_token_file() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("no XDG state/data dir")?;
    Ok(base.join("agentd").join("admin-token"))
}

/// Expand a leading `~/` (or bare `~`) to the user's home directory.
/// `~user/...` is not supported and is left as a literal path.
pub fn expand_tilde(raw: &str) -> PathBuf {
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw));
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

fn default_config_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no XDG config dir")?
        .join("agentd")
        .join("config.toml"))
}

fn default_init_file() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no XDG config dir")?
        .join("agentd")
        .join("init.lua"))
}

fn default_grants_file() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no XDG config dir")?
        .join("agentd")
        .join("grants.toml"))
}

fn default_trace_file() -> Result<PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("no XDG state/data dir")?;
    Ok(base.join("agentd").join("trace.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    // ---------- RawConfig parsing ----------

    #[test]
    fn parses_full_config_toml() {
        let src = r#"
            [daemon]
            addr = "0.0.0.0:1234"
            trace_file = "/var/log/agentd.jsonl"
            log_level = "debug"

            [runtime]
            init = "/home/me/runners/foo/init.lua"
            max_turns = 32
            yolo = false
        "#;
        let raw: RawConfig = toml::from_str(src).expect("parse");
        assert_eq!(raw.daemon.unwrap().addr.unwrap(), "0.0.0.0:1234");
        assert_eq!(raw.runtime.unwrap().max_turns.unwrap(), 32);
    }

    #[test]
    fn empty_config_toml_parses() {
        let raw: RawConfig = toml::from_str("").expect("empty parse");
        assert!(raw.daemon.is_none());
        assert!(raw.runtime.is_none());
    }

    // ---------- expand_tilde ----------

    #[test]
    fn expand_tilde_expands_home() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(expand_tilde("~/foo"), home.join("foo"));
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn expand_tilde_passes_through_absolute_paths() {
        assert_eq!(expand_tilde("/etc/hosts"), PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn expand_tilde_does_not_expand_tilde_user() {
        // `~user/...` is not supported; literal.
        assert_eq!(expand_tilde("~bob/foo"), PathBuf::from("~bob/foo"));
    }

    // ---------- Config::resolve precedence ----------

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("config.toml");
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn resolve_uses_config_toml_values() {
        let td = tempdir().unwrap();
        let cfg = write_config(
            td.path(),
            r#"
            [daemon]
            addr = "10.0.0.1:9999"
            log_level = "debug"
            [runtime]
            max_turns = 7
        "#,
        );
        let cli = Cli {
            config: Some(cfg),
            ..Cli::default()
        };
        let resolved = Config::resolve(cli).unwrap();
        assert_eq!(resolved.addr, "10.0.0.1:9999");
        assert_eq!(resolved.log_level, "debug");
        assert_eq!(resolved.max_turns, 7);
    }

    #[test]
    fn cli_overrides_config_toml() {
        let td = tempdir().unwrap();
        let cfg = write_config(
            td.path(),
            r#"
            [daemon]
            addr = "10.0.0.1:9999"
        "#,
        );
        let cli = Cli {
            config: Some(cfg),
            addr: Some("127.0.0.1:1".into()),
            ..Cli::default()
        };
        let resolved = Config::resolve(cli).unwrap();
        assert_eq!(resolved.addr, "127.0.0.1:1");
    }

    #[test]
    fn missing_config_toml_falls_back_to_defaults() {
        let td = tempdir().unwrap();
        let absent = td.path().join("does-not-exist.toml");
        let cli = Cli {
            config: Some(absent),
            ..Cli::default()
        };
        let resolved = Config::resolve(cli).unwrap();
        assert_eq!(resolved.addr, "127.0.0.1:7777");
        assert_eq!(resolved.log_level, "warn");
        assert_eq!(resolved.max_turns, 16);
        assert!(!resolved.yolo);
    }

    #[test]
    fn malformed_config_toml_errors() {
        let td = tempdir().unwrap();
        let cfg = write_config(td.path(), "this is = not valid toml [");
        let cli = Cli {
            config: Some(cfg),
            ..Cli::default()
        };
        let err = Config::resolve(cli).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("config.toml"),
            "error must mention file: {msg}"
        );
    }

    #[test]
    fn init_path_tilde_expands() {
        let td = tempdir().unwrap();
        let cfg = write_config(
            td.path(),
            r#"
            [runtime]
            init = "~/runners/foo/init.lua"
        "#,
        );
        let cli = Cli {
            config: Some(cfg),
            ..Cli::default()
        };
        let resolved = Config::resolve(cli).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(resolved.init_file, home.join("runners/foo/init.lua"));
    }
}

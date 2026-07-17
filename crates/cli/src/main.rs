//! agentctl — console client for the agentd runtime.
//!
//! Transport: WebSocket. Every command opens a single connection to `/ws`,
//! sends one JSON envelope per call, and exits when the response arrives.
//! Liveness probe (`agentctl health`) still uses plain HTTP `/health` since
//! that endpoint exists for orchestrators that won't speak WebSocket.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncSeekExt;
use tokio_tungstenite::tungstenite::Message;

#[derive(Parser, Debug)]
#[command(
    name = "agentctl",
    version,
    about = "Command-line client for a running agentd: call actions, run runners, manage grants, packages, and secrets."
)]
struct Cli {
    /// Daemon base URL.
    #[arg(
        short = 'u',
        long,
        env = "AGENTD_URL",
        default_value = "http://127.0.0.1:7777"
    )]
    url: String,

    /// Connect timeout (ms).
    #[arg(long, default_value_t = 30_000)]
    timeout: u64,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Check daemon health (HTTP probe).
    Health,
    /// List registered actions.
    Tools,
    /// Invoke an action.
    Call {
        action: String,
        #[arg(short = 'j', long)]
        json: Option<String>,
        #[arg(short = 'd', long = "data", value_name = "KEY=VAL")]
        data: Vec<String>,
        #[arg(short = 'r', long)]
        result_only: bool,
        #[arg(long)]
        compact: bool,
    },
    /// Runner operations.
    #[command(visible_alias = "runners")]
    Runner {
        #[command(subcommand)]
        cmd: RunnerCmd,
    },
    /// Skill operations.
    #[command(name = "skill", visible_alias = "skills")]
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Service operations.
    #[command(name = "service", visible_alias = "services", alias = "svc")]
    Services {
        #[command(subcommand)]
        cmd: ServicesCmd,
    },
    /// Grant management over the privileged control plane.
    Grants {
        #[command(subcommand)]
        cmd: GrantsCmd,
    },
    /// Manage installed packages (~/.local/share/agentd/packages).
    #[command(name = "package", visible_alias = "packages", alias = "pkg")]
    Packages {
        #[command(subcommand)]
        cmd: PkgCmd,
    },
    /// Manage provider API keys in the OS keyring.
    #[command(name = "secret", visible_alias = "secrets")]
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Generate lua-language-server type stubs into a project's `.luals/`.
    Types {
        /// Project directory (the folder holding `init.lua`). Defaults to the
        /// current directory.
        dir: Option<std::path::PathBuf>,
    },
    /// Tail the JSONL trace file.
    Trace {
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
}

#[derive(Subcommand, Debug)]
enum SecretCmd {
    /// Store a secret. Omit `value` to read it from stdin (keeps the key out
    /// of your shell history): `echo "$KEY" | agentctl secret set my_key`.
    Set { name: String, value: Option<String> },
    /// Remove a secret from the keyring.
    #[command(alias = "rm")]
    Unset { name: String },
    /// Show a half-obfuscated preview of a secret (never the full value).
    Peek { name: String },
}

#[derive(Subcommand, Debug)]
enum RunnerCmd {
    Ls,
    Inspect {
        name: String,
    },
    Run {
        name: String,
        prompt: String,
        #[arg(long)]
        text_only: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SkillsCmd {
    Ls,
    Inspect { name: String },
}

#[derive(Subcommand, Debug)]
enum PkgCmd {
    /// List installed packages and whether an update is available.
    Ls,
    /// Install a package from a git URL.
    Install {
        url: String,
        #[arg(long)]
        r#ref: Option<String>,
    },
    /// Re-pull and re-pin an installed package.
    Update { name: String },
    /// Remove an installed package.
    #[command(alias = "rm")]
    Remove { name: String },
}

fn packages_root() -> Result<std::path::PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| anyhow!("could not locate a data directory (XDG) on this system"))?
        .join("agentd")
        .join("packages"))
}

/// Secret management talks straight to the OS keyring (same `agentd` service
/// the daemon reads), so no daemon round-trip is needed and a running daemon
/// picks up changes on its next provider call.
fn run_secrets(cmd: SecretCmd) -> Result<()> {
    use agentd_secrets::{KeyringStore, SecretStore};
    let store = KeyringStore::default_service();
    match cmd {
        SecretCmd::Set { name, value } => {
            let value = match value {
                Some(v) => v,
                None => {
                    // Piped or typed on stdin; trailing newline stripped.
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)
                        .context("could not read the secret value from stdin")?;
                    s.trim_end_matches(['\r', '\n']).to_string()
                }
            };
            if value.is_empty() {
                return Err(anyhow!(
                    "the secret value is empty — pass it as an argument or pipe it on stdin"
                ));
            }
            store.set(&name, &value)?;
            println!("stored `{name}`");
        }
        SecretCmd::Unset { name } => {
            store.delete(&name)?;
            println!("removed `{name}`");
        }
        SecretCmd::Peek { name } => {
            let v = store.get(&name)?;
            println!("{}", obfuscate(&v));
        }
    }
    Ok(())
}

/// Half-obfuscated preview: enough to recognize a key, not enough to use
/// it. Short values are fully masked.
fn obfuscate(v: &str) -> String {
    let n = v.chars().count();
    if n < 8 {
        return format!("{} ({n} chars)", "*".repeat(n));
    }
    let head: String = v.chars().take(4).collect();
    let tail: String = v.chars().skip(n - 2).collect();
    format!("{head}{}{tail} ({n} chars)", "*".repeat((n - 6).min(12)))
}

/// Package management is a local fs + git operation — no daemon round-trip.
fn run_packages(cmd: PkgCmd) -> Result<()> {
    let root = packages_root()?;
    let index_path = root.join("index.toml");
    let mut index = agentd_packages::PackageIndex::load(&index_path).map_err(|e| anyhow!(e))?;

    match cmd {
        PkgCmd::Ls => {
            for e in index.iter() {
                let dir = root.join(&e.name);
                let upd = agentd_packages::update_check(e, &dir).unwrap_or(false);
                println!(
                    "{}\t{}\t{}{}",
                    e.name,
                    &e.commit[..e.commit.len().min(8)],
                    e.r#ref,
                    if upd { "\t(update available)" } else { "" }
                );
            }
        }
        PkgCmd::Install { url, r#ref } => {
            // Clone to a scratch dir to discover the manifest name.
            let scratch = tempfile::tempdir()?;
            agentd_packages::install(&url, r#ref.as_deref(), scratch.path(), "_probe")?;
            let manifest = agentd_packages::Manifest::load(
                &scratch.path().join("_probe").join("package.toml"),
            )?;
            let name = manifest.name.clone();
            // Real install under the manifest name.
            let entry = agentd_packages::install(&url, r#ref.as_deref(), &root, &name)?;
            index.set(entry);
            index.save(&index_path).map_err(|e| anyhow!(e))?;
            println!("installed {name}");
            if !manifest.permissions.is_empty() {
                println!(
                    "declares permissions (add `[package.{name}] trusted = true` to grants.toml to approve):"
                );
                for p in &manifest.permissions {
                    println!("  {p}");
                }
            }
        }
        PkgCmd::Update { name } => {
            let entry = index
                .get(&name)
                .ok_or_else(|| anyhow!("no package named `{name}` is installed — run `agentctl pkg ls` to see what is"))?
                .clone();
            let dir = root.join(&name);
            let commit = agentd_packages::update(&entry, &dir)?;
            let mut e = entry;
            e.commit = commit;
            index.set(e);
            index.save(&index_path).map_err(|e| anyhow!(e))?;
            println!("updated {name}");
        }
        PkgCmd::Remove { name } => {
            let dir = root.join(&name);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
            index.remove(&name);
            index.save(&index_path).map_err(|e| anyhow!(e))?;
            println!("removed {name}");
        }
    }
    Ok(())
}

#[derive(Subcommand, Debug)]
enum ServicesCmd {
    Ls,
}

#[derive(Subcommand, Debug)]
enum GrantsCmd {
    /// Listen on the control plane for permission-approval requests and
    /// answer them interactively (allow once / forever / deny).
    Listen,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Health => cmd_health(&cli.url, cli.timeout).await,
        Cmd::Tools => cmd_tools(&cli.url, cli.timeout).await,
        Cmd::Call {
            action,
            json,
            data,
            result_only,
            compact,
        } => {
            cmd_call(
                &cli.url,
                cli.timeout,
                action,
                json,
                data,
                result_only,
                compact,
            )
            .await
        }
        Cmd::Runner { cmd } => cmd_runner(&cli.url, cli.timeout, cmd).await,
        Cmd::Skills { cmd } => cmd_skills(&cli.url, cli.timeout, cmd).await,
        Cmd::Services { cmd } => cmd_services(&cli.url, cli.timeout, cmd).await,
        Cmd::Grants { cmd } => match cmd {
            GrantsCmd::Listen => cmd_grants_listen(&cli.url, cli.timeout).await,
        },
        Cmd::Packages { cmd } => run_packages(cmd),
        Cmd::Secret { cmd } => run_secrets(cmd),
        Cmd::Types { dir } => cmd_types(&cli.url, cli.timeout, dir).await,
        Cmd::Trace {
            file,
            follow,
            lines,
        } => cmd_trace(file, follow, lines).await,
    }
}

// ---------- HTTP health ----------

async fn cmd_health(base: &str, timeout: u64) -> Result<()> {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout))
        .build()?;
    let body = c.get(format!("{base}/health")).send().await?.text().await?;
    println!("{}", body.trim_end());
    Ok(())
}

// ---------- WS client ----------

fn ws_url_of(base: &str) -> Result<String> {
    let mut u = url::Url::parse(base).context("the --url value is not a valid URL")?;
    let scheme = match u.scheme() {
        "http" => "ws".to_string(),
        "https" => "wss".to_string(),
        "ws" | "wss" => u.scheme().to_string(),
        other => {
            return Err(anyhow!(
                "--url must start with http, https, ws, or wss (got `{other}`)"
            ));
        }
    };
    u.set_scheme(&scheme)
        .map_err(|_| anyhow!("could not build a WebSocket URL from --url"))?;
    u.set_path("/ws");
    Ok(u.to_string())
}

#[derive(Serialize)]
struct WsRequest<'a> {
    id: u64,
    method: &'a str,
    #[serde(skip_serializing_if = "Value::is_null")]
    params: Value,
}

#[derive(Deserialize, Debug)]
struct WsResponse {
    #[allow(dead_code)]
    id: u64,
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    tip: Option<String>,
    #[serde(default)]
    trace: Option<Vec<String>>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Bearer token for the `/ws` handshake: `AGENTD_TOKEN` wins, else the token
/// the daemon persisted to its state dir. `None` when neither exists (the
/// daemon may be running with `--no-auth`).
fn resolve_ws_token() -> Option<String> {
    if let Ok(t) = std::env::var("AGENTD_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let path = dirs::state_dir()
        .or_else(dirs::data_local_dir)?
        .join("agentd")
        .join("token");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------- control plane (grants) ----------

fn control_url_of(base: &str) -> Result<String> {
    let mut u = url::Url::parse(base).context("the --url value is not a valid URL")?;
    let scheme = match u.scheme() {
        "http" => "ws".to_string(),
        "https" => "wss".to_string(),
        "ws" | "wss" => u.scheme().to_string(),
        other => {
            return Err(anyhow!(
                "--url must start with http, https, ws, or wss (got `{other}`)"
            ));
        }
    };
    u.set_scheme(&scheme)
        .map_err(|_| anyhow!("could not build a WebSocket URL from --url"))?;
    u.set_path("/control");
    Ok(u.to_string())
}

/// Bearer token for the `/control` handshake: `AGENTD_ADMIN_TOKEN` wins, else
/// the admin token the daemon persisted to its state dir. Distinct from the
/// public `/ws` token — the control plane is operator-only.
fn resolve_admin_token() -> Option<String> {
    if let Ok(t) = std::env::var("AGENTD_ADMIN_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Some(t);
        }
    }
    let path = dirs::state_dir()
        .or_else(dirs::data_local_dir)?
        .join("agentd")
        .join("admin-token");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Connect to `/control`, subscribe, and interactively answer every approval
/// request the daemon pushes. Runs until the connection closes or Ctrl-C.
async fn cmd_grants_listen(base: &str, timeout: u64) -> Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let url = control_url_of(base)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("could not build a request for `{url}`"))?;
    if let Some(token) = resolve_admin_token() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .context("the admin token contains characters that cannot go in a header")?,
        );
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (mut ws, _) = tokio::time::timeout(Duration::from_millis(timeout), connect)
        .await
        .with_context(|| format!("timed out connecting to `{url}`"))?
        .with_context(|| {
            format!("could not connect to `{url}` — is agentd running with a control token?")
        })?;

    // Register as an approver.
    let sub = serde_json::json!({ "id": 0, "method": "approvals.subscribe" });
    ws.send(Message::Text(sub.to_string().into())).await?;
    eprintln!("listening for approval requests on {url} (Ctrl-C to stop)");

    while let Some(msg) = ws.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t.to_string(),
            Ok(Message::Binary(b)) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Message::Close(_)) => break,
            Ok(_) => continue,
            Err(e) => return Err(anyhow!("the control connection failed ({e})")),
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("event").and_then(|e| e.as_str()) != Some("approval.request") {
            continue; // acks and other responses
        }
        let req = &v["req"];
        let verdict = prompt_verdict(req).await?;
        let resolve = serde_json::json!({
            "id": NEXT_ID.fetch_add(1, Ordering::Relaxed),
            "method": "approvals.resolve",
            "params": { "request_id": req["id"], "verdict": verdict },
        });
        ws.send(Message::Text(resolve.to_string().into())).await?;
    }
    Ok(())
}

/// Render an approval request and read a one-key decision from stdin. Defaults
/// to `deny` on EOF / unrecognized input (fail safe).
async fn prompt_verdict(req: &Value) -> Result<&'static str> {
    let action = req["action"].as_str().unwrap_or("?");
    let tool = req["tool"].as_str().unwrap_or("-");
    let kind = req["kind"].as_str().unwrap_or("?");
    let reason = req["reason"].as_str().unwrap_or("");
    let missing: Vec<&str> = req["missing"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    let c = &req["caller"];
    let id_field = |k: &str| c[k].as_str().map(|s| s.to_string());
    let caller = [
        ("runner", id_field("runner")),
        ("interface", id_field("interface")),
        ("service", id_field("service")),
        ("session", id_field("session")),
        ("user", id_field("user")),
    ]
    .iter()
    .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}")))
    .collect::<Vec<_>>()
    .join(" ");

    println!(
        "\n── APPROVAL #{}  {action}  (tool: {tool}) ─────────",
        req["id"]
    );
    if !caller.is_empty() {
        println!("  caller : {caller}");
    }
    println!("  kind   : {kind}");
    if !missing.is_empty() {
        println!("  missing: {}", missing.join(", "));
    }
    if !reason.is_empty() {
        println!("  reason : {reason}");
    }
    print!("  Allow [o]nce / [f]orever / [d]eny ? ");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // Blocking stdin read off the async runtime.
    let line = tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        s
    })
    .await
    .unwrap_or_default();

    let verdict = match line.trim().chars().next() {
        Some('o') | Some('O') => "allow_once",
        Some('f') | Some('F') => "allow_forever",
        _ => "deny",
    };
    println!("  → {verdict}");
    Ok(verdict)
}

async fn ws_call(base: &str, timeout: u64, method: &str, params: Value) -> Result<WsResponse> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = ws_url_of(base)?;
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("could not build a request for `{url}`"))?;
    if let Some(token) = resolve_ws_token() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .context("the auth token contains characters that cannot go in a header")?,
        );
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (mut ws, _) = tokio::time::timeout(Duration::from_millis(timeout), connect)
        .await
        .with_context(|| format!("timed out connecting to `{url}`"))?
        .with_context(|| format!("could not connect to `{url}` — is agentd running?"))?;

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let req = WsRequest { id, method, params };
    let body = serde_json::to_string(&req)?;
    ws.send(Message::Text(body.into())).await?;

    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(t) => {
                let resp: WsResponse = serde_json::from_str(&t).with_context(|| {
                    format!("the daemon sent a response that could not be decoded ({t})")
                })?;
                let _ = ws.send(Message::Close(None)).await;
                return Ok(resp);
            }
            Message::Binary(b) => {
                let resp: WsResponse = serde_json::from_slice(&b)?;
                let _ = ws.send(Message::Close(None)).await;
                return Ok(resp);
            }
            Message::Close(_) => break,
            _ => continue,
        }
    }
    Err(anyhow!("ws closed before response"))
}

/// Generate `.luals/` type stubs for a project. Fetches this project's live
/// action / runner / skill names from the daemon (the same `*.list` methods the
/// rest of the CLI uses) and writes the static + generated stubs plus a merged
/// `.luarc.json`.
async fn cmd_types(base: &str, timeout: u64, dir: Option<std::path::PathBuf>) -> Result<()> {
    let actions = list_names(base, timeout, "tools.list").await?;
    let runners = list_names(base, timeout, "runners.list").await?;
    let skills = list_names(base, timeout, "skills.list").await?;

    let dir = match dir {
        Some(d) => d,
        None => std::env::current_dir().context("resolve current dir")?,
    };
    agentd_luals::write_project(&dir, &actions, &runners, &skills)?;
    println!(
        "wrote {}/.luals/ (agentd.lua, project.lua) and merged .luarc.json",
        dir.display()
    );
    println!(
        "  {} actions, {} runners, {} skills",
        actions.len(),
        runners.len(),
        skills.len()
    );
    Ok(())
}

/// Read a `*.list` response into a `Vec<String>` of names. `tools.list` returns
/// bare strings; `runners.list` / `skills.list` return objects with a `name`.
async fn list_names(base: &str, timeout: u64, method: &str) -> Result<Vec<String>> {
    let resp = ws_call(base, timeout, method, Value::Null).await?;
    if !resp.ok {
        let code = resp.code.as_deref().unwrap_or("error");
        let msg = resp.error.as_deref().unwrap_or("(no message)");
        return Err(anyhow!("[{code}] {msg}"));
    }
    let arr = match resp.result {
        Some(Value::Array(a)) => a,
        _ => return Ok(Vec::new()),
    };
    Ok(arr
        .into_iter()
        .filter_map(|v| match v {
            Value::String(s) => Some(s),
            Value::Object(map) => map.get("name").and_then(|n| n.as_str()).map(String::from),
            _ => None,
        })
        .collect())
}

fn print_result(resp: &WsResponse, compact: bool) -> Result<()> {
    if resp.ok {
        let v = resp.result.clone().unwrap_or(Value::Null);
        if compact {
            println!("{}", serde_json::to_string(&v)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&v)?);
        }
        Ok(())
    } else {
        let code = resp.code.as_deref().unwrap_or("error");
        let msg = resp.error.as_deref().unwrap_or("(no message)");
        if compact {
            // Machine-friendly one-liner keeps the full structured error.
            let v = serde_json::json!({
                "code": code, "error": msg, "tip": resp.tip, "trace": resp.trace,
            });
            eprintln!("{}", serde_json::to_string(&v)?);
        } else {
            use std::io::IsTerminal;
            let dim = std::io::stderr().is_terminal();
            let trace = resp.trace.as_deref().unwrap_or(&[]);
            eprintln!(
                "{}",
                render_error(code, msg, resp.tip.as_deref(), trace, dim)
            );
        }
        std::process::exit(1);
    }
}

/// Human error rendering:
///
/// ```text
/// Error: Could not resolve a provider for model `m`  (no_provider)
/// Tip: You can configure new providers in your `config.toml`
///
/// Stack trace:
///   helpers.lua:313  in structured
///   init.lua:53
/// ```
///
/// The code suffix goes ANSI-dim when `dim` (stderr is a tty). Tip and trace
/// sections only appear when present.
fn render_error(code: &str, msg: &str, tip: Option<&str>, trace: &[String], dim: bool) -> String {
    let mut msg = msg.to_string();
    if let Some(first) = msg.get(..1) {
        let up = first.to_uppercase();
        msg.replace_range(..1, &up);
    }
    let code_suffix = if dim {
        format!("  \x1b[2m({code})\x1b[0m")
    } else {
        format!("  ({code})")
    };
    let mut out = format!("Error: {msg}{code_suffix}");
    if let Some(tip) = tip {
        out.push_str(&format!("\nTip: {tip}"));
    }
    if !trace.is_empty() {
        out.push_str("\n\nStack trace:");
        for frame in trace {
            out.push_str(&format!("\n  {frame}"));
        }
    }
    out
}

// ---------- commands ----------

async fn cmd_tools(base: &str, timeout: u64) -> Result<()> {
    let resp = ws_call(base, timeout, "tools.list", Value::Null).await?;
    if resp.ok {
        if let Some(Value::Array(arr)) = resp.result {
            for v in arr {
                if let Some(s) = v.as_str() {
                    println!("{s}");
                }
            }
            return Ok(());
        }
        println!("{}", serde_json::to_string_pretty(&resp.result)?);
        Ok(())
    } else {
        print_result(&resp, false)
    }
}

async fn cmd_call(
    base: &str,
    timeout: u64,
    action: String,
    json: Option<String>,
    data: Vec<String>,
    result_only: bool,
    compact: bool,
) -> Result<()> {
    let args = build_args(json.as_deref(), &data)?;
    let params = serde_json::json!({ "name": action, "args": args });
    let resp = ws_call(base, timeout, "actions.call", params).await?;
    if resp.ok {
        let body = resp.result.clone().unwrap_or(Value::Null);
        let to_print = if result_only {
            body.get("result").cloned().unwrap_or(body)
        } else {
            body
        };
        if compact {
            println!("{}", serde_json::to_string(&to_print)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&to_print)?);
        }
        Ok(())
    } else {
        print_result(&resp, compact)
    }
}

async fn cmd_runner(base: &str, timeout: u64, cmd: RunnerCmd) -> Result<()> {
    match cmd {
        RunnerCmd::Ls => {
            let resp = ws_call(base, timeout, "runners.list", Value::Null).await?;
            if !resp.ok {
                return print_result(&resp, false);
            }
            if let Some(Value::Array(arr)) = resp.result {
                for r in arr {
                    let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("-");
                    println!("{name}\t{model}");
                }
            }
            Ok(())
        }
        RunnerCmd::Inspect { name } => {
            let resp = ws_call(
                base,
                timeout,
                "runners.inspect",
                serde_json::json!({ "name": name }),
            )
            .await?;
            print_result(&resp, false)
        }
        RunnerCmd::Run {
            name,
            prompt,
            text_only,
        } => {
            let resp = ws_call(
                base,
                timeout,
                "runners.run",
                serde_json::json!({ "name": name, "prompt": prompt }),
            )
            .await?;
            if resp.ok
                && text_only
                && let Some(t) = resp
                    .result
                    .as_ref()
                    .and_then(|r| r.get("text"))
                    .and_then(|v| v.as_str())
            {
                println!("{t}");
                return Ok(());
            }
            print_result(&resp, false)
        }
    }
}

async fn cmd_skills(base: &str, timeout: u64, cmd: SkillsCmd) -> Result<()> {
    match cmd {
        SkillsCmd::Ls => {
            let resp = ws_call(base, timeout, "skills.list", Value::Null).await?;
            if !resp.ok {
                return print_result(&resp, false);
            }
            if let Some(Value::Array(arr)) = resp.result {
                for s in arr {
                    let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let desc = s.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    println!("{name}\t{desc}");
                }
            }
            Ok(())
        }
        SkillsCmd::Inspect { name } => {
            let resp = ws_call(
                base,
                timeout,
                "skills.inspect",
                serde_json::json!({ "name": name }),
            )
            .await?;
            print_result(&resp, false)
        }
    }
}

async fn cmd_services(base: &str, timeout: u64, cmd: ServicesCmd) -> Result<()> {
    match cmd {
        ServicesCmd::Ls => {
            let resp = ws_call(base, timeout, "services.list", Value::Null).await?;
            if !resp.ok {
                return print_result(&resp, false);
            }
            if let Some(Value::Array(arr)) = resp.result {
                for s in arr {
                    let name = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let state = s.get("state").and_then(|v| v.as_str()).unwrap_or("?");
                    let err = s
                        .get("last_error")
                        .and_then(|v| v.as_str())
                        .map(|e| format!("\t{e}"))
                        .unwrap_or_default();
                    println!("{name}\t{state}{err}");
                }
            }
            Ok(())
        }
    }
}

fn build_args(json: Option<&str>, kvs: &[String]) -> Result<Value> {
    if let Some(j) = json {
        if !kvs.is_empty() {
            return Err(anyhow!("pass either --json or -d values, not both"));
        }
        return serde_json::from_str(j).context("parse --json");
    }
    if kvs.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let mut obj = serde_json::Map::new();
    for item in kvs {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| anyhow!("-d expects `key=value` pairs (got `{item}`)"))?;
        let parsed: Value =
            serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()));
        obj.insert(k.to_string(), parsed);
    }
    Ok(Value::Object(obj))
}

async fn cmd_trace(file: Option<std::path::PathBuf>, follow: bool, lines: usize) -> Result<()> {
    let path = match file {
        Some(p) => p,
        None => default_trace_path()?,
    };
    if !path.exists() {
        return Err(anyhow!(
            "no trace file at `{}` — start agentd first or pass --file",
            path.display()
        ));
    }
    let body = tokio::fs::read_to_string(&path).await?;
    let all: Vec<&str> = body.lines().collect();
    let start = all.len().saturating_sub(lines);
    for line in &all[start..] {
        println!("{line}");
    }
    if !follow {
        return Ok(());
    }
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(&path).await?;
    let mut pos = body.len() as u64;
    f.seek(std::io::SeekFrom::Start(pos)).await?;
    let mut buf = Vec::with_capacity(8 * 1024);
    loop {
        buf.clear();
        let n = f.read_to_end(&mut buf).await?;
        if n > 0 {
            let chunk = String::from_utf8_lossy(&buf);
            print!("{chunk}");
            pos += n as u64;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(meta) = tokio::fs::metadata(&path).await
            && meta.len() < pos
        {
            f = tokio::fs::File::open(&path).await?;
            pos = 0;
            f.seek(std::io::SeekFrom::Start(0)).await?;
        }
    }
}

fn default_trace_path() -> Result<std::path::PathBuf> {
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| anyhow!("no XDG state/data dir"))?;
    Ok(base.join("agentd").join("trace.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("agentctl").chain(args.iter().copied())).unwrap()
    }

    #[test]
    fn noun_aliases_parse() {
        for cmd in [
            "runner", "runners", "skill", "skills", "service", "services", "svc", "package",
            "packages", "pkg",
        ] {
            parse(&[cmd, "ls"]);
        }
    }

    #[test]
    fn secret_commands_parse() {
        assert!(matches!(
            parse(&["secret", "set", "k", "v"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Set { .. }
            }
        ));
        // Value may come from stdin.
        assert!(matches!(
            parse(&["secret", "set", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Set { value: None, .. }
            }
        ));
        assert!(matches!(
            parse(&["secrets", "unset", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Unset { .. }
            }
        ));
        assert!(matches!(
            parse(&["secret", "rm", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Unset { .. }
            }
        ));
        assert!(matches!(
            parse(&["secret", "peek", "k"]).cmd,
            Cmd::Secret {
                cmd: SecretCmd::Peek { .. }
            }
        ));
    }

    #[test]
    fn obfuscate_previews_without_revealing() {
        // Long keys: 4-char head, 2-char tail, bounded mask, length shown.
        let s = obfuscate("sk-ant-api03-abcdefghijklmnop");
        assert!(s.starts_with("sk-a"), "{s}");
        assert!(s.ends_with("op (29 chars)"), "{s}");
        assert!(!s.contains("api03"), "middle must be masked: {s}");
        // Short values: fully masked.
        assert_eq!(obfuscate("abc"), "*** (3 chars)");
        // Exactly at the boundary.
        assert_eq!(obfuscate("12345678"), "1234**78 (8 chars)");
    }

    #[test]
    fn pkg_rm_alias_parses() {
        let c = parse(&["pkg", "rm", "foo"]);
        assert!(matches!(
            c.cmd,
            Cmd::Packages {
                cmd: PkgCmd::Remove { .. }
            }
        ));
    }

    #[test]
    fn call_short_flags_parse() {
        let c = parse(&["call", "act", "-j", "{}", "-r"]);
        match c.cmd {
            Cmd::Call {
                json, result_only, ..
            } => {
                assert_eq!(json.as_deref(), Some("{}"));
                assert!(result_only);
            }
            _ => panic!("wrong cmd"),
        }
    }

    #[test]
    fn global_short_url_parses() {
        let c = parse(&["-u", "http://x:1", "health"]);
        assert_eq!(c.url, "http://x:1");
    }

    #[test]
    fn render_error_full() {
        let s = render_error(
            "no_provider",
            "could not resolve a provider for model `m`",
            Some("You can configure new providers in your `config.toml`"),
            &[
                "helpers.lua:313  in structured".to_string(),
                "init.lua:53".to_string(),
            ],
            false,
        );
        assert_eq!(
            s,
            "Error: Could not resolve a provider for model `m`  (no_provider)\nTip: You can configure new providers in your `config.toml`\n\nStack trace:\n  helpers.lua:313  in structured\n  init.lua:53"
        );
    }

    #[test]
    fn render_error_minimal() {
        let s = render_error("denied", "denied at layer 3", None, &[], false);
        assert_eq!(s, "Error: Denied at layer 3  (denied)");
    }
}

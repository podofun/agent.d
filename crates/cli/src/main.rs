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
    about = "Console client for agentd. Speaks WebSocket to the daemon."
)]
struct Cli {
    /// Daemon base URL.
    #[arg(long, env = "AGENTD_URL", default_value = "http://127.0.0.1:7777")]
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
        #[arg(long)]
        json: Option<String>,
        #[arg(short = 'd', long = "data", value_name = "KEY=VAL")]
        data: Vec<String>,
        #[arg(long)]
        result_only: bool,
        #[arg(long)]
        compact: bool,
    },
    /// Runner operations.
    Runner {
        #[command(subcommand)]
        cmd: RunnerCmd,
    },
    /// Skill operations.
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Service operations.
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
    Packages {
        #[command(subcommand)]
        cmd: PkgCmd,
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
    Remove { name: String },
}

fn packages_root() -> Result<std::path::PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| anyhow!("no data dir"))?
        .join("agentd")
        .join("packages"))
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
                .ok_or_else(|| anyhow!("{name} not installed"))?
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
    let mut u = url::Url::parse(base).context("invalid --url")?;
    let scheme = match u.scheme() {
        "http" => "ws".to_string(),
        "https" => "wss".to_string(),
        "ws" | "wss" => u.scheme().to_string(),
        other => return Err(anyhow!("unsupported scheme `{other}` in --url")),
    };
    u.set_scheme(&scheme)
        .map_err(|_| anyhow!("could not rewrite scheme"))?;
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
    let mut u = url::Url::parse(base).context("invalid --url")?;
    let scheme = match u.scheme() {
        "http" => "ws".to_string(),
        "https" => "wss".to_string(),
        "ws" | "wss" => u.scheme().to_string(),
        other => return Err(anyhow!("unsupported scheme `{other}` in --url")),
    };
    u.set_scheme(&scheme)
        .map_err(|_| anyhow!("could not rewrite scheme"))?;
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
        .with_context(|| format!("build request for {url}"))?;
    if let Some(token) = resolve_admin_token() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .context("invalid admin token")?,
        );
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (mut ws, _) = tokio::time::timeout(Duration::from_millis(timeout), connect)
        .await
        .with_context(|| format!("connect timeout to {url}"))?
        .with_context(|| {
            format!("connect to {url} (is the daemon running with a control token?)")
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
            Err(e) => return Err(anyhow!("control socket error: {e}")),
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
        .with_context(|| format!("build request for {url}"))?;
    if let Some(token) = resolve_ws_token() {
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .context("invalid auth token")?,
        );
    }
    let connect = tokio_tungstenite::connect_async(request);
    let (mut ws, _) = tokio::time::timeout(Duration::from_millis(timeout), connect)
        .await
        .with_context(|| format!("connect timeout to {url}"))?
        .with_context(|| format!("connect to {url}"))?;

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let req = WsRequest { id, method, params };
    let body = serde_json::to_string(&req)?;
    ws.send(Message::Text(body.into())).await?;

    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(t) => {
                let resp: WsResponse =
                    serde_json::from_str(&t).with_context(|| format!("decode response: {t}"))?;
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
        eprintln!("[{code}] {msg}");
        std::process::exit(1);
    }
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
            return Err(anyhow!("--json and -d are mutually exclusive"));
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
            .ok_or_else(|| anyhow!("-d expects `key=value`, got `{item}`"))?;
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
        return Err(anyhow!("trace file not found: {}", path.display()));
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

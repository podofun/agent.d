//! Command handlers for the public `/ws` plane, plus the local `types` and
//! `trace` helpers. Each maps a parsed subcommand to a daemon call and prints.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use tokio::io::AsyncSeekExt;

use crate::cli::{RunnerCmd, ServicesCmd, SkillsCmd};
use crate::render::print_result;
use crate::ws::ws_call;

// ---------- HTTP health ----------

pub(crate) async fn cmd_health(base: &str, timeout: u64) -> Result<()> {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_millis(timeout))
        .build()?;
    let body = c.get(format!("{base}/health")).send().await?.text().await?;
    println!("{}", body.trim_end());
    Ok(())
}

// ---------- WS commands ----------

pub(crate) async fn cmd_tools(base: &str, timeout: u64) -> Result<()> {
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

pub(crate) async fn cmd_call(
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

pub(crate) async fn cmd_runner(base: &str, timeout: u64, cmd: RunnerCmd) -> Result<()> {
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

pub(crate) async fn cmd_skills(base: &str, timeout: u64, cmd: SkillsCmd) -> Result<()> {
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

pub(crate) async fn cmd_services(base: &str, timeout: u64, cmd: ServicesCmd) -> Result<()> {
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

pub(crate) fn build_args(json: Option<&str>, kvs: &[String]) -> Result<Value> {
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

// ---------- types ----------

/// Generate `.luals/` type stubs for a project. Fetches this project's live
/// action / runner / skill names from the daemon (the same `*.list` methods the
/// rest of the CLI uses) and writes the static + generated stubs plus a merged
/// `.luarc.json`.
pub(crate) async fn cmd_types(
    base: &str,
    timeout: u64,
    dir: Option<std::path::PathBuf>,
) -> Result<()> {
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

// ---------- trace ----------

pub(crate) async fn cmd_trace(
    file: Option<std::path::PathBuf>,
    follow: bool,
    lines: usize,
) -> Result<()> {
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

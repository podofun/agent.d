//! The privileged `/control` plane: subscribe to permission-approval requests
//! and answer them interactively with a single keystroke.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;

use crate::ws::{NEXT_ID, token_from_env_or_file, ws_url_with_path};

/// Bearer token for the `/control` handshake: `AGENTD_ADMIN_TOKEN` wins, else
/// the admin token the daemon persisted to its state dir. Distinct from the
/// public `/ws` token — the control plane is operator-only.
fn resolve_admin_token() -> Option<String> {
    token_from_env_or_file("AGENTD_ADMIN_TOKEN", "admin-token")
}

/// Connect to `/control`, subscribe, and interactively answer every approval
/// request the daemon pushes. Runs until the connection closes or Ctrl-C.
pub(crate) async fn cmd_grants_listen(base: &str, timeout: u64) -> Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let url = ws_url_with_path(base, "/control")?;
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

// ANSI styling for the approval card. Dim for the frame/labels, bold for the
// action and the key hints so the eye lands on what matters.
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Render an approval request as a bordered card and read a single-keystroke
/// decision. Fails safe to `deny` on Ctrl-C, `q`/Esc, or a non-TTY stdin.
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

    // A left-barred card. No right border, so unicode-width padding never
    // misaligns; each row is `│  label   value`.
    let mut rows = vec![("tool", tool.to_string())];
    if !caller.is_empty() {
        rows.push(("caller", caller));
    }
    rows.push(("kind", kind.to_string()));
    if !missing.is_empty() {
        rows.push(("missing", missing.join(", ")));
    }
    if !reason.is_empty() {
        rows.push(("reason", reason.to_string()));
    }

    let id = &req["id"];
    println!("\n  {DIM}╭╴{RESET} approval {DIM}#{id}{RESET} · {BOLD}{action}{RESET}");
    for (label, value) in &rows {
        println!("  {DIM}│{RESET}  {DIM}{label:<8}{RESET}{value}");
    }
    print!("  {DIM}╰╴{RESET} {BOLD}o{RESET} once   {BOLD}f{RESET} forever   {BOLD}d{RESET} deny  {DIM}›{RESET} ");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // One-key read off the async runtime. crossterm raw mode gives a
    // cross-platform single keystroke; a non-TTY stdin (pipe, test) falls
    // back to a line read so scripted approvals still work.
    let verdict = tokio::task::spawn_blocking(read_verdict_key)
        .await
        .unwrap_or("deny");

    // Collapse the whole card into one line summarizing the decision. On a TTY,
    // rewind over the card (header + rows + prompt) and clear it first; when
    // piped there's nothing to rewind, so just print the summary below.
    // Rewind over the rows + prompt line to the header; the leading blank line
    // stays as a separator above the summary.
    let card_lines = rows.len() as u16 + 1;
    collapse_card(card_lines);
    println!("{}", decision_summary(verdict, action, tool));
    Ok(verdict)
}

/// One-line recap of a resolved approval, e.g.
/// `✓ You approved git.status to use git · once`.
fn decision_summary(verdict: &str, action: &str, tool: &str) -> String {
    match verdict {
        "allow_once" => {
            format!("  {GREEN}✓{RESET} You approved {BOLD}{action}{RESET} to use {tool} {DIM}· once{RESET}")
        }
        "allow_forever" => {
            format!("  {GREEN}✓{RESET} You approved {BOLD}{action}{RESET} to use {tool} {DIM}· always{RESET}")
        }
        _ => format!("  {RED}✗{RESET} You denied {BOLD}{action}{RESET} {DIM}({tool}){RESET}"),
    }
}

/// Rewind `lines` rows up to the top of the printed card and clear everything
/// below, so the caller can reprint a single summary line in its place. No-op
/// when stdout is not a terminal (piped output has nothing to rewind).
fn collapse_card(lines: u16) {
    use std::io::{IsTerminal, stdout};
    if !stdout().is_terminal() {
        return;
    }
    use crossterm::cursor::MoveToPreviousLine;
    use crossterm::terminal::{Clear, ClearType};
    let _ = crossterm::execute!(stdout(), MoveToPreviousLine(lines), Clear(ClearType::FromCursorDown));
}

/// Block on a single keystroke and map it to a verdict. `o`/`f` allow, anything
/// else (including `d`, `q`, Esc, Ctrl-C) denies. Falls back to a line read when
/// raw mode is unavailable (stdin is not a terminal).
fn read_verdict_key() -> &'static str {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    if enable_raw_mode().is_err() {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        return match s.trim().chars().next() {
            Some('o' | 'O') => "allow_once",
            Some('f' | 'F') => "allow_forever",
            _ => "deny",
        };
    }

    let verdict = loop {
        match crossterm::event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                let ctrl_c =
                    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
                match k.code {
                    KeyCode::Char('o' | 'O') => break "allow_once",
                    KeyCode::Char('f' | 'F') => break "allow_forever",
                    KeyCode::Char('d' | 'D') | KeyCode::Char('q') | KeyCode::Esc => break "deny",
                    _ if ctrl_c => break "deny",
                    _ => continue, // ignore stray keys; keep waiting
                }
            }
            Ok(_) => continue,
            Err(_) => break "deny",
        }
    };
    let _ = disable_raw_mode();
    verdict
}

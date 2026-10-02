//! ClaudeCliProvider tests.
//!
//! All happy-path tests hit the real `claude` CLI and are gated by:
//!   AGENTD_TEST_CLAUDE=1
//! They cost tokens. CI/dev runs default-skip them.
//!
//! Sad-path tests that don't need the real binary stay always-on.

use agentd_ai::{ClaudeCliProvider, CompletionRequest, Provider, ProviderError};

/// Tests in this file spawn processes, and two of them first write the script
/// they spawn. A process forked by one test while another is still writing its
/// script briefly holds that script open for writing, and running it then
/// fails with "Text file busy". Every spawning test holds this lock so writing
/// and spawning never overlap.
static SPAWN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CLAUDE").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn missing_binary_is_transport_error() {
    let _spawn = SPAWN.lock().await;
    let p = ClaudeCliProvider::new().with_bin("/nonexistent/agentd-test-binary");
    let err = p
        .complete(CompletionRequest::prompt("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Transport(_)), "got {err:?}");
}

#[tokio::test]
async fn live_basic() {
    let _spawn = SPAWN.lock().await;
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1");
        return;
    }
    let p = ClaudeCliProvider::new();
    let res = p
        .complete(CompletionRequest::prompt(
            "Reply with exactly the single word: pong",
        ))
        .await
        .expect("live claude call");
    assert!(!res.text.is_empty(), "empty response");
    assert!(
        res.text.to_lowercase().contains("pong"),
        "expected `pong`, got: {}",
        res.text
    );
}

#[tokio::test]
async fn live_with_system_prompt() {
    let _spawn = SPAWN.lock().await;
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1");
        return;
    }
    let p = ClaudeCliProvider::new();
    let res = p
        .complete(
            CompletionRequest::prompt("What number are you supposed to say?")
                .with_system("You always reply with exactly the digit `7` and nothing else."),
        )
        .await
        .expect("live claude call");
    assert!(res.text.contains('7'), "expected `7`, got: {}", res.text);
}

#[tokio::test]
async fn live_with_model_flag() {
    let _spawn = SPAWN.lock().await;
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1");
        return;
    }
    let p = ClaudeCliProvider::new();
    let res = p
        .complete(
            CompletionRequest::prompt("Reply with exactly: ok")
                .with_model("claude-haiku-4-5-20251001"),
        )
        .await
        .expect("live claude call with model flag");
    assert!(!res.text.is_empty(), "empty response");
}

/// A stand-in `claude` that writes the arguments it was launched with, one per
/// line, to the file named by its own path plus `.args`, then prints a final
/// result event so the provider has something to parse.
#[cfg(unix)]
fn recording_claude() -> (tempfile::TempDir, String, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("claude");
    let log = dir.path().join("claude.args");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{}'\ncat > /dev/null\nprintf '%s\\n' '{{\"type\":\"result\",\"result\":\"ok\"}}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin = bin.to_string_lossy().into_owned();
    (dir, bin, log)
}

/// The claude CLI must run with none of its own tools, none of the user's MCP
/// servers, and none of the user's or project's settings files, so a runner
/// can only act through agentd actions.
#[cfg(unix)]
fn assert_confined(args: &[String]) {
    let has_pair = |flag: &str, value: &str| args.windows(2).any(|w| w[0] == flag && w[1] == value);
    assert!(has_pair("--tools", ""), "built-in tools disabled: {args:?}");
    assert!(
        has_pair("--setting-sources", ""),
        "settings files ignored: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "--strict-mcp-config"),
        "only agentd's MCP server: {args:?}"
    );
}

#[cfg(unix)]
fn read_args(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn text_only_calls_run_without_claude_tools_or_settings() {
    let _spawn = SPAWN.lock().await;
    let (_dir, bin, log) = recording_claude();
    let p = ClaudeCliProvider::new().with_bin(bin);
    p.complete(CompletionRequest::prompt("hi")).await.unwrap();
    assert_confined(&read_args(&log));
}

#[cfg(unix)]
#[tokio::test]
async fn tool_loop_calls_run_with_only_agentd_tools() {
    let _spawn = SPAWN.lock().await;
    let (_dir, bin, log) = recording_claude();
    let p = ClaudeCliProvider::new().with_bin(bin);
    let mut req = CompletionRequest::prompt("hi");
    req.mcp_endpoint = Some(agentd_ai::McpEndpoint::Http {
        url: "http://127.0.0.1:9/mcp".into(),
        token: "t".into(),
    });
    p.complete(req).await.unwrap();
    let args = read_args(&log);
    assert_confined(&args);
    assert!(
        args.windows(2)
            .any(|w| w[0] == "--allowedTools" && w[1] == "mcp__agentd__*"),
        "agentd tools still allowed: {args:?}"
    );
}

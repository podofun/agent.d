//! ClaudeCliProvider tests.
//!
//! All happy-path tests hit the real `claude` CLI and are gated by:
//!   AGENTD_TEST_CLAUDE=1
//! They cost tokens. CI/dev runs default-skip them.
//!
//! Sad-path tests that don't need the real binary stay always-on.

use agentd_ai::{ClaudeCliProvider, CompletionRequest, Provider, ProviderError};

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CLAUDE").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn missing_binary_is_transport_error() {
    let p = ClaudeCliProvider::new().with_bin("/nonexistent/agentd-test-binary");
    let err = p
        .complete(CompletionRequest::prompt("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Transport(_)), "got {err:?}");
}

#[tokio::test]
async fn live_basic() {
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

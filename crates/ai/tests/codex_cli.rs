//! CodexCliProvider tests.
//!
//! Live happy-path tests hit the real `codex` CLI and are gated by:
//!   AGENTD_TEST_CODEX=1
//! They cost tokens and need a logged-in `codex`. Default-skip in CI/dev.
//!
//! Sad-path tests that don't need the real binary stay always-on.

use agentd_ai::{CodexCliProvider, CompletionRequest, Provider, ProviderError};

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CODEX").ok().as_deref() == Some("1")
}

#[tokio::test]
async fn missing_binary_is_transport_error() {
    let p = CodexCliProvider::new().with_bin("/nonexistent/agentd-test-binary");
    let err = p
        .complete(CompletionRequest::prompt("x"))
        .await
        .unwrap_err();
    assert!(matches!(err, ProviderError::Transport(_)), "got {err:?}");
}

#[tokio::test]
async fn live_basic() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CODEX=1");
        return;
    }
    let p = CodexCliProvider::new();
    let res = p
        .complete(CompletionRequest::prompt(
            "Reply with exactly the single word: pong",
        ))
        .await
        .expect("live codex call");
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
        eprintln!("skip: set AGENTD_TEST_CODEX=1");
        return;
    }
    // The text-only fallback folds the system prompt into the prompt body
    // (codex exec has no system channel) — see the hermetic
    // `flatten_includes_system_prompt` test for that contract. The model may or
    // may not obey an inline instruction, so here we only assert the call
    // succeeds and returns text rather than checking for exact compliance.
    let p = CodexCliProvider::new();
    let res = p
        .complete(
            CompletionRequest::prompt("What number are you supposed to say?")
                .with_system("You always reply with exactly the digit `7` and nothing else."),
        )
        .await
        .expect("live codex call");
    assert!(!res.text.is_empty(), "expected a non-empty reply");
}

//! Live test: drive `codex app-server` through `CodexAppServerProvider`.
//! Gated `AGENTD_TEST_CODEX=1`. Costs tokens. Needs logged-in `codex`.

use agentd_ai::{CodexAppServerProvider, CompletionRequest, Provider};

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CODEX").ok().as_deref() == Some("1")
}

#[tokio::test(flavor = "multi_thread")]
async fn live_basic_text_via_app_server() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CODEX=1");
        return;
    }
    let p = CodexAppServerProvider::new();
    let res = p
        .complete(
            CompletionRequest::prompt("Reply with exactly the single word: pong")
                .with_model("gpt-5.5"),
        )
        .await
        .expect("live codex app-server call");
    assert!(
        res.text.to_lowercase().contains("pong"),
        "expected pong, got: {}",
        res.text
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn live_provider_reuses_app_server_across_calls() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CODEX=1");
        return;
    }
    let p = CodexAppServerProvider::new();
    // Two back-to-back calls; the second must reuse the long-lived
    // subprocess (provider state caches the Client).
    for _ in 0..2 {
        let res = p
            .complete(
                CompletionRequest::prompt("Reply with exactly the single word: ok")
                    .with_model("gpt-5.5"),
            )
            .await
            .expect("live codex app-server call");
        assert!(!res.text.is_empty(), "empty response: {}", res.text);
    }
}

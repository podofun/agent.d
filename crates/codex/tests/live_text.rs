//! Live smoke test: spawn real `codex app-server`, initialize, open a
//! thread, run one turn, read final assistant text. No tools, no
//! approvals — just confirms the transport works end-to-end.
//!
//! Gated `AGENTD_TEST_CODEX=1`. Costs tokens. Needs logged-in `codex`.

use agentd_codex::{
    Client, ClientInfo, Inbound, InitializeParams, ThreadStartParams, ThreadStartResult,
    TurnStartParams, UserInput,
};

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CODEX").ok().as_deref() == Some("1")
}

#[tokio::test(flavor = "multi_thread")]
async fn live_initialize_thread_turn_text() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CODEX=1");
        return;
    }

    let (client, mut inbox) = Client::spawn("codex").await.expect("spawn app-server");

    let init = client
        .request(
            "initialize",
            serde_json::to_value(InitializeParams {
                client_info: ClientInfo {
                    name: "agentd".into(),
                    version: "0.1.0".into(),
                },
            })
            .unwrap(),
        )
        .await
        .expect("initialize");
    assert!(
        init.get("userAgent").is_some(),
        "initialize result missing userAgent: {init}"
    );

    let thread_resp = client
        .request(
            "thread/start",
            serde_json::to_value(ThreadStartParams {
                model: Some("gpt-5.5".into()),
                sandbox: Some("read-only".into()),
                approval_policy: Some("on-request".into()),
                ephemeral: Some(true),
                ..Default::default()
            })
            .unwrap(),
        )
        .await
        .expect("thread/start");
    let parsed: ThreadStartResult =
        serde_json::from_value(thread_resp).expect("thread start result shape");
    let tid = parsed.thread.id;

    client
        .request(
            "turn/start",
            serde_json::to_value(TurnStartParams {
                thread_id: tid.clone(),
                input: vec![UserInput::text("Reply with exactly the single word: pong")],
                model: None,
                effort: None,
            })
            .unwrap(),
        )
        .await
        .expect("turn/start");

    // Collect assistant text from item/completed agentMessage events
    // until turn/completed.
    let mut final_text = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("timeout before turn/completed; collected: {final_text:?}");
        }
        let ev = tokio::time::timeout(std::time::Duration::from_secs(45), inbox.recv())
            .await
            .expect("inbox recv timeout");
        let Some(ev) = ev else {
            panic!("inbox closed");
        };
        match ev {
            Inbound::Notification { method, params } => {
                if method == "item/completed"
                    && let Some(item) = params.get("item")
                    && item.get("type").and_then(|t| t.as_str()) == Some("agentMessage")
                    && let Some(text) = item.get("text").and_then(|t| t.as_str())
                {
                    if !final_text.is_empty() {
                        final_text.push('\n');
                    }
                    final_text.push_str(text);
                }
                if method == "turn/completed" {
                    break;
                }
                if method == "error" {
                    panic!("codex error notification: {params}");
                }
            }
            Inbound::ServerRequest { id, method, .. } => {
                // We didn't expect any server requests in a text-only
                // run, but answer them defensively so the test doesn't
                // wedge if codex elicits something.
                client.reply(id, serde_json::json!({})).await.ok();
                eprintln!("unexpected server request: {method}");
            }
        }
    }
    assert!(
        final_text.to_lowercase().contains("pong"),
        "expected `pong`, got: {final_text}"
    );

    client.shutdown().await.ok();
}

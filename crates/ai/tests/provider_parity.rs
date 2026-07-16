//! Cross-provider parity contract: the Lua-facing surface must be identical
//! regardless of provider. The SAME scenario runs against a fake Anthropic
//! endpoint and a fake OpenAI-compatible endpoint returning semantically
//! identical replies; the normalized `CompletionResponse`s must match on
//! every provider-agnostic field. A divergence here is a bug — wire-format
//! vocabulary must not leak past the `Provider` trait.

use std::net::SocketAddr;
use std::sync::Arc;

use agentd_ai::{
    ClaudeApiProvider, CompletionRequest, CompletionResponse, OpenAiApiProvider, Provider, ToolDef,
};
use agentd_secrets::MemoryStore;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Response as HRes, StatusCode};
use hyper_util::rt::TokioIo;

async fn spawn_canned(canned: serde_json::Value) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let canned = canned.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let canned = canned.clone();
                    async move {
                        let _ = req.collect().await;
                        let resp: HRes<Full<Bytes>> = HRes::builder()
                            .status(StatusCode::OK)
                            .header("content-type", "application/json")
                            .body(Full::new(Bytes::from(canned.to_string())))
                            .unwrap();
                        Ok::<_, std::convert::Infallible>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    addr
}

async fn anthropic_provider(canned: serde_json::Value) -> ClaudeApiProvider {
    let addr = spawn_canned(canned).await;
    ClaudeApiProvider::new(Arc::new(MemoryStore::default()))
        .with_endpoint(format!("http://{addr}"))
        .with_no_auth()
}

async fn openai_provider(canned: serde_json::Value) -> OpenAiApiProvider {
    let addr = spawn_canned(canned).await;
    OpenAiApiProvider::new(Arc::new(MemoryStore::default()))
        .with_endpoint(format!("http://{addr}/v1"))
        .with_no_auth()
}

/// The provider-agnostic projection of a response — every field Lua can see.
fn normalized(r: &CompletionResponse) -> serde_json::Value {
    serde_json::json!({
        "text": r.text,
        "stop_reason": r.stop_reason,
        "tool_calls": r.tool_calls.iter().map(|tc| {
            serde_json::json!({ "name": tc.name, "arguments": tc.arguments })
        }).collect::<Vec<_>>(),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_text_turn_is_identical_across_providers() {
    let anthropic = anthropic_provider(serde_json::json!({
        "model": "m",
        "stop_reason": "end_turn",
        "content": [{ "type": "text", "text": "hello back" }],
    }))
    .await;
    let openai = openai_provider(serde_json::json!({
        "model": "m",
        "choices": [{
            "finish_reason": "stop",
            "message": { "content": "hello back" },
        }],
    }))
    .await;

    let req = || CompletionRequest::prompt("hello");
    let a = anthropic.complete(req()).await.unwrap();
    let o = openai.complete(req()).await.unwrap();
    assert_eq!(normalized(&a), normalized(&o));
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_turn_is_identical_across_providers() {
    let anthropic = anthropic_provider(serde_json::json!({
        "model": "m",
        "stop_reason": "tool_use",
        "content": [
            { "type": "text", "text": "checking" },
            { "type": "tool_use", "id": "c1", "name": "notes.lookup", "input": { "q": "x" } },
        ],
    }))
    .await;
    let openai = openai_provider(serde_json::json!({
        "model": "m",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "content": "checking",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "notes.lookup", "arguments": "{\"q\":\"x\"}" },
                }],
            },
        }],
    }))
    .await;

    let req = || CompletionRequest {
        prompt: Some("look up x".into()),
        tools: vec![ToolDef {
            name: "notes.lookup".into(),
            description: Some("read note".into()),
            input_schema: serde_json::json!({ "type": "object" }),
        }],
        ..Default::default()
    };
    let a = anthropic.complete(req()).await.unwrap();
    let o = openai.complete(req()).await.unwrap();
    assert_eq!(normalized(&a), normalized(&o));
}

#[tokio::test(flavor = "multi_thread")]
async fn max_tokens_stop_is_identical_across_providers() {
    let anthropic = anthropic_provider(serde_json::json!({
        "model": "m",
        "stop_reason": "max_tokens",
        "content": [{ "type": "text", "text": "truncat" }],
    }))
    .await;
    let openai = openai_provider(serde_json::json!({
        "model": "m",
        "choices": [{
            "finish_reason": "length",
            "message": { "content": "truncat" },
        }],
    }))
    .await;

    let req = || CompletionRequest::prompt("hello");
    let a = anthropic.complete(req()).await.unwrap();
    let o = openai.complete(req()).await.unwrap();
    assert_eq!(normalized(&a), normalized(&o));
}

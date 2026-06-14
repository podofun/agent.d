//! Spins a tiny local hyper server that impersonates the Anthropic
//! Messages endpoint, points `ClaudeApiProvider` at it, and asserts the
//! request body shape + response decoding.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentd_ai::{
    ClaudeApiProvider, CompletionRequest, LoopMode, Message, Provider, Role, ToolCall, ToolDef,
};
use agentd_secrets::{MemoryStore, SecretStore};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Response as HRes, StatusCode};
use hyper_util::rt::TokioIo;

#[derive(Debug, Default, Clone)]
struct Captured {
    headers: BTreeMap<String, String>,
    body: serde_json::Value,
}

async fn spawn_fake_anthropic(
    canned: serde_json::Value,
) -> (SocketAddr, Arc<Mutex<Vec<Captured>>>) {
    let log: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log_t = log.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let log = log_t.clone();
            let canned = canned.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let log = log.clone();
                    let canned = canned.clone();
                    async move {
                        let mut headers = BTreeMap::new();
                        for (k, v) in req.headers() {
                            if let Ok(s) = v.to_str() {
                                headers.insert(k.as_str().to_string(), s.to_string());
                            }
                        }
                        let body = req.collect().await.unwrap().to_bytes();
                        let parsed: serde_json::Value =
                            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                        log.lock().unwrap().push(Captured {
                            headers,
                            body: parsed,
                        });
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
    (addr, log)
}

fn provider_for(endpoint: &str, key: &str) -> ClaudeApiProvider {
    let store = Arc::new(MemoryStore::default());
    store.set("anthropic_api_key", key).unwrap();
    ClaudeApiProvider::new(store).with_endpoint(endpoint)
}

#[tokio::test(flavor = "multi_thread")]
async fn round_trip_text_response() {
    let canned = serde_json::json!({
        "id": "msg_1",
        "model": "claude-opus-4-7",
        "stop_reason": "end_turn",
        "content": [
            { "type": "text", "text": "hello back" }
        ]
    });
    let (addr, log) = spawn_fake_anthropic(canned).await;
    let p = provider_for(&format!("http://{addr}/v1/messages"), "sk-test");
    assert_eq!(p.loop_mode(), LoopMode::ExecutorOwned);

    let req = CompletionRequest {
        system: Some("be terse".into()),
        messages: vec![Message::user("hi")],
        model: Some("claude-opus-4-7".into()),
        max_tokens: Some(200),
        ..Default::default()
    };
    let resp = p.complete(req).await.unwrap();
    assert_eq!(resp.text, "hello back");
    assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
    assert!(resp.tool_calls.is_empty());

    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let cap = &captured[0];
    assert_eq!(cap.headers.get("x-api-key").unwrap(), "sk-test");
    assert_eq!(cap.headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert_eq!(cap.body["system"], "be terse");
    assert_eq!(cap.body["model"], "claude-opus-4-7");
    assert_eq!(cap.body["max_tokens"], 200);
    assert_eq!(cap.body["messages"][0]["role"], "user");
}

#[tokio::test(flavor = "multi_thread")]
async fn round_trip_tool_use_response() {
    let canned = serde_json::json!({
        "id": "msg_2",
        "model": "claude-opus-4-7",
        "stop_reason": "tool_use",
        "content": [
            { "type": "text", "text": "let me look" },
            { "type": "tool_use", "id": "tu_1", "name": "notes.lookup", "input": { "q": "x" } }
        ]
    });
    let (addr, _log) = spawn_fake_anthropic(canned).await;
    let p = provider_for(&format!("http://{addr}/v1/messages"), "sk-test");

    let req = CompletionRequest {
        messages: vec![Message::user("look up x")],
        tools: vec![ToolDef {
            name: "notes.lookup".into(),
            description: Some("read a note".into()),
            input_schema: serde_json::json!({ "type": "object" }),
        }],
        ..Default::default()
    };
    let resp = p.complete(req).await.unwrap();
    assert_eq!(resp.text, "let me look");
    assert_eq!(resp.tool_calls.len(), 1);
    let tc: &ToolCall = &resp.tool_calls[0];
    assert_eq!(tc.id, "tu_1");
    assert_eq!(tc.name, "notes.lookup");
    assert_eq!(tc.arguments, serde_json::json!({ "q": "x" }));
}

#[tokio::test(flavor = "multi_thread")]
async fn upstream_error_surfaces() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
                    let resp: HRes<Full<Bytes>> = HRes::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(Full::new(Bytes::from("{\"error\":\"nope\"}")))
                        .unwrap();
                    Ok::<_, std::convert::Infallible>(resp)
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });

    let p = provider_for(&format!("http://{addr}/v1/messages"), "sk-test");
    let err = p
        .complete(CompletionRequest::prompt("x"))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("400") || msg.contains("nope"), "got {msg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_result_turn_serialized_as_user_role_with_tool_result_block() {
    // After the executor dispatches a tool, it appends a `Role::Tool`
    // message. The provider must translate it into a `user` role message
    // carrying a `tool_result` content block.
    let canned = serde_json::json!({
        "id": "msg_3",
        "model": "claude-opus-4-7",
        "stop_reason": "end_turn",
        "content": [{ "type": "text", "text": "done" }]
    });
    let (addr, log) = spawn_fake_anthropic(canned).await;
    let p = provider_for(&format!("http://{addr}/v1/messages"), "sk-test");

    let req = CompletionRequest {
        messages: vec![
            Message::user("read note"),
            Message {
                role: Role::Assistant,
                content: "looking".into(),
                tool_calls: vec![ToolCall {
                    id: "tu_1".into(),
                    name: "notes.lookup".into(),
                    arguments: serde_json::json!({ "q": "x" }),
                }],
                tool_call_id: None,
            },
            Message::tool_result("tu_1", "{\"found\":\"x=42\"}"),
        ],
        ..Default::default()
    };
    p.complete(req).await.unwrap();

    let captured = log.lock().unwrap();
    let msgs = captured[0].body["messages"].as_array().unwrap();
    // Three turns: user, assistant (w/ tool_use), user (tool_result).
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
    assert_eq!(msgs[2]["content"][0]["tool_use_id"], "tu_1");
}

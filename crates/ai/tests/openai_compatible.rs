//! Spins a tiny local hyper server that impersonates an OpenAI-compatible
//! Chat Completions endpoint (OpenRouter/Groq/vLLM/Ollama shape), points
//! `OpenAiApiProvider` at it, and asserts auth headers, default-model
//! fallback, and tool-call round-trips.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentd_ai::{CompletionRequest, Message, OpenAiApiProvider, Provider, ToolDef};
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

async fn spawn_fake_openai(canned: serde_json::Value) -> (SocketAddr, Arc<Mutex<Vec<Captured>>>) {
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

fn text_reply(text: &str) -> serde_json::Value {
    serde_json::json!({
        "model": "test-model",
        "choices": [{
            "finish_reason": "stop",
            "message": { "content": text },
        }],
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn no_auth_sends_no_authorization_header() {
    let (addr, log) = spawn_fake_openai(text_reply("hi")).await;
    let secrets = Arc::new(MemoryStore::default());
    let p = OpenAiApiProvider::new(secrets)
        .with_name("ollama")
        .with_endpoint(format!("http://{addr}/v1"))
        .with_no_auth();

    let res = p.complete(CompletionRequest::prompt("hello")).await.unwrap();
    assert_eq!(res.text, "hi");

    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(
        !captured[0].headers.contains_key("authorization"),
        "no-auth provider must not send an Authorization header: {:?}",
        captured[0].headers
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn keyed_provider_sends_bearer_token() {
    let (addr, log) = spawn_fake_openai(text_reply("hi")).await;
    let secrets = Arc::new(MemoryStore::default());
    secrets.set("openrouter_api_key", "sk-test").unwrap();
    let p = OpenAiApiProvider::new(secrets)
        .with_name("openrouter")
        .with_endpoint(format!("http://{addr}/v1"))
        .with_secret_key("openrouter_api_key");

    p.complete(CompletionRequest::prompt("hello")).await.unwrap();

    let captured = log.lock().unwrap();
    assert_eq!(
        captured[0].headers.get("authorization").map(String::as_str),
        Some("Bearer sk-test")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn default_model_fills_requests_without_one() {
    let (addr, log) = spawn_fake_openai(text_reply("hi")).await;
    let secrets = Arc::new(MemoryStore::default());
    let p = OpenAiApiProvider::new(secrets)
        .with_endpoint(format!("http://{addr}/v1"))
        .with_no_auth()
        .with_default_model("qwen3:14b");

    p.complete(CompletionRequest::prompt("hello")).await.unwrap();

    let captured = log.lock().unwrap();
    assert_eq!(captured[0].body["model"], "qwen3:14b");
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_round_trip_decodes_arguments() {
    let canned = serde_json::json!({
        "model": "test-model",
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "notes.lookup", "arguments": "{\"q\":\"x\"}" },
                }],
            },
        }],
    });
    let (addr, log) = spawn_fake_openai(canned).await;
    let secrets = Arc::new(MemoryStore::default());
    let p = OpenAiApiProvider::new(secrets)
        .with_endpoint(format!("http://{addr}/v1"))
        .with_no_auth();

    let req = CompletionRequest {
        prompt: Some("look up x".into()),
        tools: vec![ToolDef {
            name: "notes.lookup".into(),
            description: Some("read note".into()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "q": { "type": "string" } },
                "required": ["q"],
            }),
        }],
        ..Default::default()
    };
    let res = p.complete(req).await.unwrap();
    assert_eq!(res.tool_calls.len(), 1);
    assert_eq!(res.tool_calls[0].name, "notes.lookup");
    assert_eq!(res.tool_calls[0].arguments["q"], "x");
    assert_eq!(res.stop_reason.as_deref(), Some("tool_calls"));

    // The input schema traveled to the wire as function.parameters.
    let captured = log.lock().unwrap();
    let sent = &captured[0].body["tools"][0]["function"]["parameters"];
    assert_eq!(sent["required"][0], "q");

    // Feed the tool result back — second turn must succeed.
    drop(captured);
    let follow_up = CompletionRequest {
        messages: vec![
            Message::user("look up x"),
            Message {
                role: agentd_ai::Role::Assistant,
                content: String::new(),
                tool_calls: res.tool_calls.clone(),
                tool_call_id: None,
            },
            Message::tool_result("c1", r#"{"found":"x=42"}"#),
        ],
        ..Default::default()
    };
    let secrets = Arc::new(MemoryStore::default());
    let (addr2, log2) = spawn_fake_openai(text_reply("x is 42")).await;
    let p2 = OpenAiApiProvider::new(secrets)
        .with_endpoint(format!("http://{addr2}/v1"))
        .with_no_auth();
    let res2 = p2.complete(follow_up).await.unwrap();
    assert_eq!(res2.text, "x is 42");
    let captured2 = log2.lock().unwrap();
    let msgs = captured2[0].body["messages"].as_array().unwrap();
    assert_eq!(msgs.last().unwrap()["role"], "tool");
    assert_eq!(msgs.last().unwrap()["tool_call_id"], "c1");
}

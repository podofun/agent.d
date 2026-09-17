//! End-to-end WebSocket envelope tests for the daemon control plane. Boots the
//! real router on an ephemeral port, backed by a real `Executor` over a
//! `LuaHost` with one registered action, and drives it from a tungstenite
//! client — the same path agentctl takes.

use std::collections::HashMap;
use std::sync::Arc;

use agentd_api::{AppState, router, serve};
use agentd_executor::Executor;
use agentd_permissions::{Engine, Grants, GrantsFile};
use agentd_scripting::LuaHost;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::Registry;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

/// Boot with `echo.ping` and no auth; returns the `ws://` URL.
async fn boot() -> String {
    boot_with_auth(None).await.0
}

/// Boot the router on 127.0.0.1:0 with a single `echo.ping` action. Returns the
/// `ws://` URL plus the configured auth token (if any).
async fn boot_with_auth(auth_token: Option<String>) -> (String, Option<String>) {
    let host = LuaHost::new().expect("lua host");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("echo.lua"),
        r#"
        agentd.action("echo.ping", function(args)
          return { pong = true, got = args }
        end)
        "#,
    )
    .unwrap();
    host.load_dir(dir.path()).expect("load tool");
    // Keep the temp dir alive for the process — leaking is fine in a test.
    std::mem::forget(dir);

    let skills = host.skills();
    let runners = host.runners();
    let services = host.services();
    let registry: Arc<dyn Registry> = Arc::new(host);
    let executor = Arc::new(Executor::new(
        registry,
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(GrantsFile::default()))),
        runners,
        services,
        skills,
        Arc::new(agentd_ai::ProviderRegistry::new()),
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState {
        lifecycle: Arc::new(agentd_api::Lifecycle::default()),
        executor: Arc::new(arc_swap::ArcSwap::from(executor)),
        auth_token: auth_token.clone().map(Arc::new),
        admin_token: None,
        interface_tokens: Arc::new(HashMap::new()),
        broker: Arc::new(agentd_approvals::Broker::new(
            std::time::Duration::from_secs(30),
        )),
        webhooks: Arc::new(HashMap::new()),
    };
    tokio::spawn(async move {
        let _ = serve(listener, router(state)).await;
    });
    (format!("ws://{addr}/ws"), auth_token)
}

/// Send one request envelope, return the decoded response envelope.
async fn call(
    sock: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    req: serde_json::Value,
) -> serde_json::Value {
    sock.send(Message::Text(req.to_string().into()))
        .await
        .unwrap();
    loop {
        match sock.next().await.unwrap().unwrap() {
            Message::Text(t) => return serde_json::from_str(&t).unwrap(),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    }
}

#[tokio::test]
async fn health_tools_and_action_roundtrip() {
    let url = boot().await;
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    let health = call(&mut sock, serde_json::json!({"id": 1, "method": "health"})).await;
    assert_eq!(health["id"], 1);
    assert_eq!(health["ok"], true);
    assert_eq!(health["result"], "ok");

    let tools = call(
        &mut sock,
        serde_json::json!({"id": 2, "method": "tools.list"}),
    )
    .await;
    assert_eq!(tools["ok"], true);
    let names: Vec<String> =
        serde_json::from_value(tools["result"].clone()).expect("tools list is an array");
    assert!(names.contains(&"echo.ping".to_string()), "got {names:?}");

    let res = call(
        &mut sock,
        serde_json::json!({
            "id": 3,
            "method": "actions.call",
            "params": { "name": "echo.ping", "args": { "x": 7 } }
        }),
    )
    .await;
    assert_eq!(res["ok"], true, "envelope: {res}");
    assert_eq!(res["result"]["result"]["pong"], true);
    assert_eq!(res["result"]["result"]["got"]["x"], 7);
    assert!(res["result"]["duration_ms"].is_number());
}

#[tokio::test]
async fn auth_token_gates_the_handshake() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let (url, token) = boot_with_auth(Some("s3cret".into())).await;
    let token = token.unwrap();

    // No Authorization header → handshake rejected (401).
    assert!(
        tokio_tungstenite::connect_async(&url).await.is_err(),
        "unauthenticated connect should be refused"
    );

    // Correct bearer → handshake succeeds and the session works.
    let mut req = url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut sock, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let health = call(&mut sock, serde_json::json!({"id": 1, "method": "health"})).await;
    assert_eq!(health["result"], "ok");
}

#[tokio::test]
async fn unknown_method_and_missing_action_error_cleanly() {
    let url = boot().await;
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    let unknown = call(
        &mut sock,
        serde_json::json!({"id": 9, "method": "nope.method"}),
    )
    .await;
    assert_eq!(unknown["ok"], false);
    assert_eq!(unknown["code"], "unknown_method");

    let missing = call(
        &mut sock,
        serde_json::json!({
            "id": 10,
            "method": "actions.call",
            "params": { "name": "does.not.exist" }
        }),
    )
    .await;
    assert_eq!(missing["ok"], false);
    assert_eq!(missing["code"], "not_found");
}

/// Boot with a mock-backed runner and no auth; returns the `ws://` URL.
async fn boot_with_runner() -> String {
    boot_runner(
        Arc::new(agentd_ai::MockProvider::new().with_reply("streamed hello")),
        true,
    )
    .await
    .0
}

async fn boot_runner(
    provider: Arc<dyn agentd_ai::Provider>,
    grant: bool,
) -> (String, Arc<agentd_api::Lifecycle>) {
    boot_runner_with_interfaces(provider, grant, HashMap::new()).await
}

/// Like `boot_runner`, with the public token disabled and the given
/// `token -> interface` map, so tests can connect as distinct interfaces.
async fn boot_runner_with_interfaces(
    provider: Arc<dyn agentd_ai::Provider>,
    grant: bool,
    interface_tokens: HashMap<String, String>,
) -> (String, Arc<agentd_api::Lifecycle>) {
    let host = LuaHost::new().expect("lua host");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("runner.lua"),
        r#"
        agentd.runner({ name = "helper", model = "mock/test" })
        "#,
    )
    .unwrap();
    host.load_dir(dir.path()).expect("load runner");
    std::mem::forget(dir);

    let skills = host.skills();
    let runners = host.runners();
    let services = host.services();
    let registry: Arc<dyn Registry> = Arc::new(host);
    let mut providers = agentd_ai::ProviderRegistry::new();
    providers.insert("mock", provider.clone());
    providers.insert("other", provider);
    let mut grants = GrantsFile::default();
    if grant {
        grants.runner.insert(
            "helper".into(),
            agentd_permissions::RunnerGrants {
                granted: agentd_permissions::PermissionSet::from_iter(["ai:mock"]),
                ..Default::default()
            },
        );
    }
    providers.set_default("mock");
    let mut executor = Executor::new(
        registry,
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(grants))),
        runners,
        services,
        skills,
        Arc::new(providers),
    );
    executor.set_sessions(Arc::new(agentd_sessions::MemSessionStore::new()));
    let executor = Arc::new(executor);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let lifecycle = Arc::new(agentd_api::Lifecycle::default());
    let state = AppState {
        lifecycle: lifecycle.clone(),
        executor: Arc::new(arc_swap::ArcSwap::from(executor)),
        // With interfaces configured, auth is on: the public token is `ws`.
        auth_token: (!interface_tokens.is_empty()).then(|| Arc::new("public-token".to_string())),
        admin_token: None,
        interface_tokens: Arc::new(interface_tokens),
        broker: Arc::new(agentd_approvals::Broker::new(
            std::time::Duration::from_secs(30),
        )),
        webhooks: Arc::new(HashMap::new()),
    };
    tokio::spawn(async move {
        let _ = serve(listener, router(state)).await;
    });
    (format!("ws://{addr}/ws"), lifecycle)
}

/// Connect to `url` presenting `token` as the bearer.
async fn connect_as(url: &str, token: &str) -> Socket {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

#[tokio::test]
async fn streaming_run_pushes_deltas_then_complete_response() {
    let url = boot_with_runner().await;
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    sock.send(Message::Text(
        serde_json::json!({
            "id": 9,
            "method": "runners.run",
            "params": { "name": "helper", "prompt": "hi", "stream": true }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let mut deltas: Vec<serde_json::Value> = Vec::new();
    let final_resp = loop {
        match sock.next().await.unwrap().unwrap() {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v.get("event").and_then(|e| e.as_str()) == Some("runner.delta") {
                    assert_eq!(v["id"], 9, "delta frames carry the request id");
                    deltas.push(v["delta"].clone());
                    continue;
                }
                break v;
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("unexpected frame: {other:?}"),
        }
    };

    // Every delta frame arrived before the final response; text deltas
    // reassemble the full reply the complete envelope also carries.
    let streamed: String = deltas
        .iter()
        .filter(|d| d["type"] == "text_delta")
        .filter_map(|d| d["text"].as_str())
        .collect();
    assert_eq!(streamed, "streamed hello");
    assert!(deltas.iter().any(|d| d["type"] == "turn_end"));
    assert_eq!(final_resp["id"], 9);
    assert_eq!(final_resp["ok"], true);
    assert_eq!(final_resp["result"]["text"], "streamed hello");
}

#[tokio::test]
async fn non_streaming_run_gets_single_complete_response() {
    let url = boot_with_runner().await;
    let (mut sock, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let resp = call(
        &mut sock,
        serde_json::json!({
            "id": 10,
            "method": "runners.run",
            "params": { "name": "helper", "prompt": "hi" }
        }),
    )
    .await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["result"]["text"], "streamed hello");
}

struct ControlledProvider {
    requests: std::sync::Mutex<Vec<agentd_ai::CompletionRequest>>,
    started: tokio::sync::Notify,
    stopped: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ControlledProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: std::sync::Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
            stopped: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
}

#[async_trait]
impl agentd_ai::Provider for ControlledProvider {
    fn name(&self) -> &str {
        "mock"
    }
    async fn complete(
        &self,
        req: agentd_ai::CompletionRequest,
    ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
        let wait = req.messages.last().is_some_and(|m| m.content == "wait");
        if req
            .messages
            .last()
            .is_some_and(|m| m.content == "rate_limit")
        {
            return Err(agentd_ai::ProviderError::Http {
                status: 429,
                message: "rate limited".into(),
                retry_after_ms: Some(2000),
            });
        }
        self.requests.lock().unwrap().push(req);
        if wait {
            struct Stopped<'a>(&'a tokio::sync::Notify);
            impl Drop for Stopped<'_> {
                fn drop(&mut self) {
                    self.0.notify_one();
                }
            }
            let _stopped = Stopped(&self.stopped);
            self.started.notify_one();
            self.release.notified().await;
        }
        Ok(agentd_ai::MockProvider::text_only("done"))
    }
    async fn complete_streaming(
        &self,
        req: agentd_ai::CompletionRequest,
        sink: agentd_ai::StreamSink,
    ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
        if req.messages.last().is_some_and(|m| m.content == "burst") {
            for _ in 0..200 {
                let _ = sink.send(agentd_ai::StreamEvent::TextDelta { text: "x".into() });
            }
        }
        self.complete(req).await
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn receive(socket: &mut Socket) -> serde_json::Value {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Message::Text(text) = socket.next().await.unwrap().unwrap() {
                return serde_json::from_str(&text).unwrap();
            }
        }
    })
    .await
    .expect("response deadline")
}

async fn send_request(socket: &mut Socket, value: serde_json::Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn runner_permission_and_model_override_are_checked_before_provider_access() {
    for (granted, model) in [(false, "mock/test"), (true, "other/test")] {
        let provider = ControlledProvider::new();
        let (url, _) = boot_runner(provider.clone(), granted).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        for stream in [false, true] {
            let response = call(&mut socket, serde_json::json!({"id":1,"method":"runners.run","params":{"name":"helper","prompt":"hello","model":model,"stream":stream}})).await;
            assert_eq!(response["code"], "denied", "{response}");
        }
        assert!(provider.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn history_and_limits_reach_provider_and_invalid_history_is_rejected() {
    let provider = ControlledProvider::new();
    let (url, _) = boot_runner(provider.clone(), true).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let history = serde_json::json!([
        {"role":"user","content":"remember 42"},
        {"role":"assistant","content":"checking","tool_calls":[{"id":"t1","name":"notes.read","arguments":{}}]},
        {"role":"tool","content":"42","tool_call_id":"t1"}
    ]);
    let response = call(&mut socket, serde_json::json!({"id":1,"method":"runners.run","params":{"name":"helper","messages":history,"prompt":"what was it?","max_tokens":80}})).await;
    assert_eq!(response["ok"], true, "{response}");
    {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests[0].messages.len(), 4);
        assert_eq!(requests[0].messages[1].tool_calls[0].id, "t1");
        assert_eq!(requests[0].messages[2].tool_call_id.as_deref(), Some("t1"));
        assert_eq!(requests[0].max_tokens, Some(80));
    }
    for extra in [
        serde_json::json!({"messages":[{"role":"typo","content":"hello"}]}),
        serde_json::json!({"messages":[{"role":"tool","content":"secret","tool_call_id":"missing"}]}),
        serde_json::json!({"prompt":"hello","max_tokens":0}),
        serde_json::json!({"prompt":"hello","timeout_ms":600001}),
        serde_json::json!({"prompt":"hello","typo":true}),
        serde_json::json!({"prompt":" "}),
    ] {
        let mut params = extra;
        params["name"] = "helper".into();
        let response = call(
            &mut socket,
            serde_json::json!({"id":2,"method":"runners.run","params":params}),
        )
        .await;
        assert_eq!(response["code"], "bad_params", "{response}");
    }
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn slow_runner_does_not_block_health_or_another_runner_and_can_be_cancelled() {
    let provider = ControlledProvider::new();
    let (url, _) = boot_runner(provider.clone(), true).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    send_request(&mut socket, serde_json::json!({"id":1,"method":"runners.run","params":{"name":"helper","prompt":"wait","stream":true}})).await;
    provider.started.notified().await;
    send_request(&mut socket, serde_json::json!({"id":2,"method":"health"})).await;
    assert_eq!(receive(&mut socket).await["id"], 2);
    send_request(&mut socket, serde_json::json!({"id":3,"method":"runners.run","params":{"name":"helper","prompt":"hello"}})).await;
    assert_eq!(receive(&mut socket).await["id"], 3);
    send_request(
        &mut socket,
        serde_json::json!({"id":4,"method":"runners.cancel","params":{"id":1}}),
    )
    .await;
    let first = receive(&mut socket).await;
    let second = receive(&mut socket).await;
    let replies = [first, second];
    assert!(
        replies
            .iter()
            .any(|r| r["id"] == 1 && r["code"] == "cancelled")
    );
    assert!(
        replies
            .iter()
            .any(|r| r["id"] == 4 && r["result"]["cancelled"] == true)
    );
    provider.stopped.notified().await;
}

#[tokio::test]
async fn timeout_and_disconnect_drop_provider_work() {
    for disconnect in [false, true] {
        let provider = ControlledProvider::new();
        let (url, _) = boot_runner(provider.clone(), true).await;
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        send_request(&mut socket, serde_json::json!({"id":1,"method":"runners.run","params":{"name":"helper","prompt":"wait","timeout_ms":100}})).await;
        provider.started.notified().await;
        if disconnect {
            socket.close(None).await.unwrap();
        } else {
            assert_eq!(receive(&mut socket).await["code"], "timeout");
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            provider.stopped.notified(),
        )
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn drain_finishes_existing_runner_and_rejects_new_connections() {
    let provider = ControlledProvider::new();
    let (url, lifecycle) = boot_runner(provider.clone(), true).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    send_request(&mut socket, serde_json::json!({"id":1,"method":"runners.run","params":{"name":"helper","prompt":"wait"}})).await;
    provider.started.notified().await;
    let draining = lifecycle.clone();
    let drain = tokio::spawn(async move {
        draining.drain(std::time::Duration::from_secs(3)).await;
    });
    while !lifecycle.is_draining() {
        tokio::task::yield_now().await;
    }
    assert!(tokio_tungstenite::connect_async(&url).await.is_err());
    provider.release.notify_one();
    assert_eq!(receive(&mut socket).await["ok"], true);
    tokio::time::timeout(std::time::Duration::from_secs(1), drain)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn connection_admission_is_bounded_and_capacity_is_reusable() {
    let provider = ControlledProvider::new();
    let (url, _) = boot_runner(provider.clone(), true).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    for id in 1..=33 {
        send_request(&mut socket, serde_json::json!({"id":id,"method":"runners.run","params":{"name":"helper","prompt":"wait"}})).await;
    }
    let response = receive(&mut socket).await;
    assert_eq!(response["id"], 33);
    assert_eq!(response["code"], "busy");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while provider.requests.lock().unwrap().len() < 32 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    provider.release.notify_waiters();
    for _ in 0..32 {
        assert_eq!(receive(&mut socket).await["ok"], true);
    }
    let result = call(&mut socket, serde_json::json!({"id":34,"method":"runners.run","params":{"name":"helper","prompt":"hello"}})).await;
    assert_eq!(result["ok"], true);
}

#[tokio::test]
async fn provider_http_metadata_survives_the_websocket_envelope() {
    let (url, _) = boot_runner(ControlledProvider::new(), true).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let response = call(&mut socket, serde_json::json!({"id":1,"method":"runners.run","params":{"name":"helper","prompt":"rate_limit"}})).await;
    assert_eq!(response["code"], "provider_upstream");
    assert_eq!(response["provider_status"], 429);
    assert_eq!(response["retry_after_ms"], 2000);
}

#[tokio::test]
async fn burst_stream_drains_in_order_without_false_slow_consumer_failure() {
    let (url, _) = boot_runner(ControlledProvider::new(), true).await;
    let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    send_request(&mut socket, serde_json::json!({"id":1,"method":"runners.run","params":{"name":"helper","prompt":"burst","stream":true}})).await;
    for _ in 0..200 {
        assert_eq!(receive(&mut socket).await["event"], "runner.delta");
    }
    assert_eq!(receive(&mut socket).await["ok"], true);
}

#[tokio::test]
async fn sessions_roundtrip_over_ws() {
    let provider = ControlledProvider::new();
    let (url, _) = boot_runner(provider.clone(), true).await;
    let (mut sock, _) = tokio_tungstenite::connect_async(url).await.unwrap();

    let created = call(
        &mut sock,
        serde_json::json!({"id": 1, "method": "sessions.create", "params": {"label": "tg-1", "user": "bob"}}),
    )
    .await;
    assert_eq!(created["ok"], true, "{created}");
    let sid = created["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(created["result"]["label"], "tg-1");
    assert_eq!(created["result"]["owner"], "interface:ws");
    assert_eq!(created["result"]["user"], "bob");

    let dup = call(
        &mut sock,
        serde_json::json!({"id": 2, "method": "sessions.create", "params": {"label": "tg-1", "user": "bob"}}),
    )
    .await;
    assert_eq!(dup["ok"], false);
    assert_eq!(dup["code"], "session_label_taken");

    // The session belongs to `bob`; a run without that user cannot see it.
    let anon = call(
        &mut sock,
        serde_json::json!({"id": 3, "method": "runners.run", "params": {"name": "helper", "prompt": "first", "session_id": sid}}),
    )
    .await;
    assert_eq!(anon["code"], "session_not_found", "{anon}");

    let run = call(
        &mut sock,
        serde_json::json!({"id": 4, "method": "runners.run", "params": {"name": "helper", "prompt": "first", "session_id": sid, "user": "bob"}}),
    )
    .await;
    assert_eq!(run["ok"], true, "{run}");
    assert_eq!(run["result"]["session_id"], sid);

    let run2 = call(
        &mut sock,
        serde_json::json!({"id": 5, "method": "runners.run", "params": {"name": "helper", "prompt": "second", "session_id": sid, "user": "bob"}}),
    )
    .await;
    assert_eq!(run2["ok"], true, "{run2}");
    {
        let reqs = provider.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        let hist: Vec<&str> = reqs[1]
            .messages
            .iter()
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(hist[0], "first");
        assert_eq!(hist[2], "second");
    }

    let by_label = call(
        &mut sock,
        serde_json::json!({"id": 6, "method": "sessions.get", "params": {"label": "tg-1", "user": "bob"}}),
    )
    .await;
    assert_eq!(by_label["ok"], true, "{by_label}");
    assert_eq!(by_label["result"]["id"], sid);
    assert_eq!(by_label["result"]["turn_count"], 4);
    assert_eq!(by_label["result"]["turns"].as_array().unwrap().len(), 4);

    let missing = call(
        &mut sock,
        serde_json::json!({"id": 7, "method": "runners.run", "params": {"name": "helper", "prompt": "x", "session_id": "nope", "user": "bob"}}),
    )
    .await;
    assert_eq!(missing["code"], "session_not_found");
    assert!(
        missing["tip"]
            .as_str()
            .unwrap()
            .contains("agentctl session")
    );

    let both = call(
        &mut sock,
        serde_json::json!({"id": 8, "method": "runners.run", "params": {"name": "helper", "prompt": "x", "session_id": sid, "messages": [], "user": "bob"}}),
    )
    .await;
    assert_eq!(both["code"], "bad_params");

    // list: bob sees his session, an unnamed caller sees nothing.
    let list = call(
        &mut sock,
        serde_json::json!({"id": 9, "method": "sessions.list", "params": {"user": "bob"}}),
    )
    .await;
    assert_eq!(list["result"].as_array().unwrap().len(), 1);
    let list_anon = call(
        &mut sock,
        serde_json::json!({"id": 10, "method": "sessions.list", "params": {}}),
    )
    .await;
    assert_eq!(list_anon["result"].as_array().unwrap().len(), 0);

    // delete by someone else is a no-op that looks like "already gone".
    let del_eve = call(
        &mut sock,
        serde_json::json!({"id": 11, "method": "sessions.delete", "params": {"id": sid, "user": "eve"}}),
    )
    .await;
    assert_eq!(del_eve["result"]["deleted"], false);
    let del = call(
        &mut sock,
        serde_json::json!({"id": 12, "method": "sessions.delete", "params": {"id": sid, "user": "bob"}}),
    )
    .await;
    assert_eq!(del["result"]["deleted"], true);
    let gone = call(
        &mut sock,
        serde_json::json!({"id": 13, "method": "sessions.get", "params": {"id": sid, "user": "bob"}}),
    )
    .await;
    assert_eq!(gone["code"], "session_not_found");
}

#[tokio::test]
async fn interface_tokens_isolate_sessions_between_interfaces() {
    let provider = ControlledProvider::new();
    let tokens = HashMap::from([
        ("web-token".to_string(), "webapp".to_string()),
        ("tg-token".to_string(), "telegram".to_string()),
    ]);
    let (url, _) = boot_runner_with_interfaces(provider.clone(), true, tokens).await;

    // An unknown token is refused even though the public token is disabled.
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut req = url.as_str().into_client_request().unwrap();
        req.headers_mut()
            .insert("authorization", "Bearer nope".parse().unwrap());
        assert!(tokio_tungstenite::connect_async(req).await.is_err());
    }

    let mut web = connect_as(&url, "web-token").await;
    let mut tg = connect_as(&url, "tg-token").await;
    // The public token is still interface `ws`.
    let mut pub_sock = connect_as(&url, "public-token").await;
    let p = call(
        &mut pub_sock,
        serde_json::json!({"id": 1, "method": "sessions.create", "params": {}}),
    )
    .await;
    assert_eq!(p["result"]["owner"], "interface:ws", "{p}");

    // Same label on both interfaces → two distinct sessions, each owned by its interface.
    let w = call(
        &mut web,
        serde_json::json!({"id": 1, "method": "sessions.create", "params": {"label": "chat-1", "user": "alice"}}),
    )
    .await;
    let t = call(
        &mut tg,
        serde_json::json!({"id": 1, "method": "sessions.create", "params": {"label": "chat-1", "user": "alice"}}),
    )
    .await;
    assert_eq!(w["ok"], true, "{w}");
    assert_eq!(t["ok"], true, "{t}");
    assert_eq!(w["result"]["owner"], "interface:webapp");
    assert_eq!(t["result"]["owner"], "interface:telegram");
    let wid = w["result"]["id"].as_str().unwrap().to_string();
    let tid = t["result"]["id"].as_str().unwrap().to_string();
    assert_ne!(wid, tid);

    // alice on webapp writes a secret into her session.
    let run = call(
        &mut web,
        serde_json::json!({"id": 2, "method": "runners.run", "params": {"name": "helper", "prompt": "PIN 4242", "session_id": wid, "user": "alice"}}),
    )
    .await;
    assert_eq!(run["ok"], true, "{run}");

    // telegram, even claiming to be alice, cannot read, run on, list, or delete it.
    let steal_get = call(
        &mut tg,
        serde_json::json!({"id": 2, "method": "sessions.get", "params": {"id": wid, "user": "alice"}}),
    )
    .await;
    assert_eq!(steal_get["code"], "session_not_found");
    let steal_run = call(
        &mut tg,
        serde_json::json!({"id": 3, "method": "runners.run", "params": {"name": "helper", "prompt": "what pin?", "session_id": wid, "user": "alice"}}),
    )
    .await;
    assert_eq!(steal_run["code"], "session_not_found");
    let tg_list = call(
        &mut tg,
        serde_json::json!({"id": 4, "method": "sessions.list", "params": {"user": "alice"}}),
    )
    .await;
    let ids: Vec<&str> = tg_list["result"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![tid.as_str()]);
    let steal_del = call(
        &mut tg,
        serde_json::json!({"id": 5, "method": "sessions.delete", "params": {"id": wid, "user": "alice"}}),
    )
    .await;
    assert_eq!(steal_del["result"]["deleted"], false);

    // Within webapp, bob cannot read alice's session either.
    let bob = call(
        &mut web,
        serde_json::json!({"id": 3, "method": "sessions.get", "params": {"id": wid, "user": "bob"}}),
    )
    .await;
    assert_eq!(bob["code"], "session_not_found");
    // ...but alice can, by label too.
    let alice = call(
        &mut web,
        serde_json::json!({"id": 4, "method": "sessions.get", "params": {"label": "chat-1", "user": "alice"}}),
    )
    .await;
    assert_eq!(alice["result"]["id"], wid);
    assert_eq!(alice["result"]["turns"][0]["content"], "PIN 4242");

    // The provider only ever saw alice's own run.
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
}

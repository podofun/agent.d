//! End-to-end WebSocket envelope tests for the daemon control plane. Boots the
//! real router on an ephemeral port, backed by a real `Executor` over a
//! `LuaHost` with one registered action, and drives it from a tungstenite
//! client — the same path agentctl takes.

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
        executor: Arc::new(arc_swap::ArcSwap::from(executor)),
        auth_token: auth_token.clone().map(Arc::new),
        admin_token: None,
        broker: Arc::new(agentd_approvals::Broker::new(
            std::time::Duration::from_secs(30),
        )),
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

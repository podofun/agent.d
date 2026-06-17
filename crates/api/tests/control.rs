//! Control-plane (`/control`) tests: admin-token gating, the approval
//! subscribe → push → resolve round trip, and plane isolation (public token
//! cannot reach `/control`).

use std::sync::Arc;
use std::time::Duration;

use agentd_api::{AppState, router, serve};
use agentd_approvals::Broker;
use agentd_executor::Executor;
use agentd_permissions::{Engine, Grants, GrantsFile};
use agentd_scripting::LuaHost;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{ApprovalBroker, ApprovalKind, ApprovalRequest, Registry, Verdict};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

struct Booted {
    addr: String,
    admin: String,
    public: String,
    broker: Arc<Broker>,
}

async fn boot() -> Booted {
    let host = LuaHost::new().expect("lua host");
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
    let broker = Arc::new(Broker::new(Duration::from_secs(30)));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState {
        executor: Arc::new(arc_swap::ArcSwap::from(executor)),
        auth_token: Some(Arc::new("public-tok".into())),
        admin_token: Some(Arc::new("admin-tok".into())),
        broker: broker.clone(),
    };
    tokio::spawn(async move {
        let _ = serve(listener, router(state)).await;
    });
    Booted {
        addr: format!("ws://{addr}"),
        admin: "admin-tok".into(),
        public: "public-tok".into(),
        broker,
    }
}

fn req_with_bearer(
    url: &str,
    token: &str,
) -> tokio_tungstenite::tungstenite::handshake::client::Request {
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    req
}

#[tokio::test]
async fn control_rejects_without_admin_token() {
    let b = boot().await;
    let url = format!("{}/control", b.addr);
    // No header at all.
    assert!(tokio_tungstenite::connect_async(&url).await.is_err());
    // Wrong token.
    assert!(
        tokio_tungstenite::connect_async(req_with_bearer(&url, "nope"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn control_rejects_public_token() {
    let b = boot().await;
    let url = format!("{}/control", b.addr);
    // The public token must NOT open the control plane.
    assert!(
        tokio_tungstenite::connect_async(req_with_bearer(&url, &b.public))
            .await
            .is_err(),
        "public token reached /control"
    );
}

#[tokio::test]
async fn subscribe_push_resolve_roundtrip() {
    let b = boot().await;
    let url = format!("{}/control", b.addr);
    let (mut sock, _) = tokio_tungstenite::connect_async(req_with_bearer(&url, &b.admin))
        .await
        .unwrap();

    // Subscribe (idempotent ack).
    sock.send(Message::Text(
        serde_json::json!({"id": 1, "method": "approvals.subscribe"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let ack: serde_json::Value = loop {
        match sock.next().await.unwrap().unwrap() {
            Message::Text(t) => break serde_json::from_str(&t).unwrap(),
            _ => continue,
        }
    };
    assert_eq!(ack["ok"], true);

    // Fire an approval request through the broker from another task.
    let broker = b.broker.clone();
    let handle = tokio::spawn(async move {
        broker
            .request(ApprovalRequest {
                id: 42,
                kind: ApprovalKind::MissingGrant,
                action: "tool.act".into(),
                tool: Some("tool".into()),
                requires: vec!["cap:foo".into()],
                missing: vec!["cap:foo".into()],
                reason: "needs cap:foo".into(),
                caller: Default::default(),
            })
            .await
    });

    // Expect the push frame.
    let push: serde_json::Value = loop {
        match sock.next().await.unwrap().unwrap() {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v.get("event").is_some() {
                    break v;
                }
            }
            _ => continue,
        }
    };
    assert_eq!(push["event"], "approval.request");
    assert_eq!(push["req"]["id"], 42);
    assert_eq!(push["req"]["missing"][0], "cap:foo");

    // Resolve allow_once.
    sock.send(Message::Text(
        serde_json::json!({
            "id": 2,
            "method": "approvals.resolve",
            "params": { "request_id": 42, "verdict": "allow_once" }
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let verdict = handle.await.unwrap();
    assert_eq!(verdict, Verdict::AllowOnce);
}

#[tokio::test]
async fn public_ws_still_works_with_public_token() {
    let b = boot().await;
    let url = format!("{}/ws", b.addr);
    let (mut sock, _) = tokio_tungstenite::connect_async(req_with_bearer(&url, &b.public))
        .await
        .unwrap();
    sock.send(Message::Text(
        serde_json::json!({"id": 1, "method": "health"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let resp: serde_json::Value = loop {
        match sock.next().await.unwrap().unwrap() {
            Message::Text(t) => break serde_json::from_str(&t).unwrap(),
            _ => continue,
        }
    };
    assert_eq!(resp["result"], "ok");
}

//! End-to-end tests for signed webhook ingestion.

use std::collections::HashMap;
use std::sync::Arc;

use agentd_api::{AppState, Webhook, router, serve};
use agentd_executor::Executor;
use agentd_permissions::{Engine, Grants, GrantsFile, InterfaceGrants};
use agentd_scripting::LuaHost;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::Registry;
use async_trait::async_trait;
use hmac::{Hmac, Mac};
use reqwest::StatusCode;
use sha2::Sha256;

struct NullSink;

#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _event: TraceEvent) {}
}

async fn boot(action_body: &str, id_header: Option<&str>) -> String {
    boot_with_grants(action_body, id_header, GrantsFile::default()).await
}

async fn boot_with_grants(
    action_body: &str,
    id_header: Option<&str>,
    grants: GrantsFile,
) -> String {
    let host = LuaHost::new().expect("lua host");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("webhook.lua"),
        format!(
            r#"
            agentd.action("webhook.ingest", function(args, ctx)
              {action_body}
            end)
            "#
        ),
    )
    .unwrap();
    host.load_dir(dir.path()).expect("load action");
    std::mem::forget(dir);

    let skills = host.skills();
    let runners = host.runners();
    let services = host.services();
    let registry: Arc<dyn Registry> = Arc::new(host);
    let executor = Arc::new(Executor::new(
        registry,
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(grants))),
        runners,
        services,
        skills,
        Arc::new(agentd_ai::ProviderRegistry::new()),
    ));
    let webhooks = HashMap::from([(
        "events".to_string(),
        Webhook::new(
            "webhook.ingest",
            "correct horse battery staple",
            "x-test-signature",
            "hmac-sha256=",
            id_header,
        )
        .unwrap(),
    )]);
    let state = AppState {
        executor: Arc::new(arc_swap::ArcSwap::from(executor)),
        auth_token: None,
        admin_token: None,
        broker: Arc::new(agentd_approvals::Broker::new(
            std::time::Duration::from_secs(30),
        )),
        webhooks: Arc::new(webhooks),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = serve(listener, router(state)).await;
    });
    format!("http://{addr}")
}

fn signature(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(b"correct horse battery staple").unwrap();
    mac.update(body);
    format!("hmac-sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn deliver(base: &str, body: &[u8], signature: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/webhooks/events"))
        .header("X-Test-Signature", signature)
        .header("X-Event-Type", "pull_request")
        .header("X-Request-Id", "request-123")
        .header("Content-Type", "application/json")
        .body(body.to_vec())
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn valid_delivery_dispatches_with_verified_identity_and_payload() {
    let base = boot(
        r#"
        assert(ctx.caller.interface == "webhook.events")
        assert(ctx.caller.session == "request-123")
        assert(ctx.caller.execution ~= nil)
        assert(args.headers["x-event-type"] == "pull_request")
        assert(args.headers["x-request-id"] == "request-123")
        assert(args.headers["x-test-signature"] == nil)
        assert(args.payload.action == "opened")
        return { accepted = true }
        "#,
        Some("x-request-id"),
    )
    .await;
    let body = br#"{"action":"opened","number":42}"#;
    let response = deliver(&base, body, &signature(body)).await;

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let response: serde_json::Value =
        serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["request_id"], "request-123");
}

#[tokio::test]
async fn missing_or_invalid_signature_is_rejected() {
    let base = boot("return { accepted = true }", Some("x-request-id")).await;
    let body = br#"{"action":"opened"}"#;
    let missing = reqwest::Client::new()
        .post(format!("{base}/webhooks/events"))
        .header("X-Event-Type", "pull_request")
        .header("X-Request-Id", "request-123")
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let invalid = deliver(
        &base,
        body,
        "hmac-sha256=0000000000000000000000000000000000000000000000000000000000000000",
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signed_non_json_body_is_rejected() {
    let base = boot("return { accepted = true }", Some("x-request-id")).await;
    let body = b"not json";
    let response = deliver(&base, body, &signature(body)).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn configured_id_header_is_required() {
    let base = boot("return { accepted = true }", Some("x-request-id")).await;
    let body = br#"{"action":"opened"}"#;
    let response = reqwest::Client::new()
        .post(format!("{base}/webhooks/events"))
        .header("X-Test-Signature", signature(body))
        .body(body.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn route_without_id_header_generates_request_id() {
    let base = boot(
        r#"
        assert(string.startswith(ctx.caller.session, "webhook-events-"))
        return { accepted = true }
        "#,
        None,
    )
    .await;
    let body = br#"{"action":"opened"}"#;
    let response = deliver(&base, body, &signature(body)).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let response: serde_json::Value =
        serde_json::from_str(&response.text().await.unwrap()).unwrap();
    assert!(
        response["request_id"]
            .as_str()
            .unwrap()
            .starts_with("webhook-events-")
    );
}

#[tokio::test]
async fn unknown_route_and_action_failure_do_not_leak_details() {
    let base = boot(r#"error("internal details")"#, Some("x-request-id")).await;
    let unknown = reqwest::Client::new()
        .post(format!("{base}/webhooks/unknown"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    let body = br#"{"action":"opened"}"#;
    let failed = deliver(&base, body, &signature(body)).await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let text = failed.text().await.unwrap();
    assert!(
        !text.contains("internal details"),
        "response leaked: {text}"
    );
}

#[tokio::test]
async fn webhook_interface_allowlist_is_enforced() {
    let mut grants = GrantsFile::default();
    grants.interface.insert(
        "webhook.events".into(),
        InterfaceGrants {
            allowed_actions: ["other.action".to_string()].into_iter().collect(),
            ..Default::default()
        },
    );
    let base = boot_with_grants("return { accepted = true }", Some("x-request-id"), grants).await;
    let body = br#"{"action":"opened"}"#;
    let response = deliver(&base, body, &signature(body)).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

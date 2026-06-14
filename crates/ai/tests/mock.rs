use agentd_ai::{CompletionRequest, Message, MockProvider, Provider};

#[tokio::test]
async fn mock_echoes_prompt() {
    let p = MockProvider::new();
    let res = p
        .complete(CompletionRequest::prompt("hello"))
        .await
        .unwrap();
    assert_eq!(res.text, "hello");
}

#[tokio::test]
async fn mock_canned_reply() {
    let p = MockProvider::new().with_reply("canned");
    let res = p
        .complete(CompletionRequest::prompt("ignored"))
        .await
        .unwrap();
    assert_eq!(res.text, "canned");
}

#[tokio::test]
async fn mock_flattens_system_and_messages() {
    let p = MockProvider::new();
    let req = CompletionRequest {
        system: Some("be terse".into()),
        messages: vec![Message::user("ping"), Message::assistant("pong")],
        prompt: Some("again".into()),
        ..Default::default()
    };
    let res = p.complete(req).await.unwrap();
    assert!(res.text.contains("be terse"));
    assert!(res.text.contains("[user] ping"));
    assert!(res.text.contains("[assistant] pong"));
    assert!(res.text.ends_with("again"));
}

#[tokio::test]
async fn provider_name() {
    let p = MockProvider::new();
    assert_eq!(p.name(), "mock");
}

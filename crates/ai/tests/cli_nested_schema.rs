use std::sync::{Arc, Mutex};

use agentd_ai::{
    ClaudeCliProvider, CodexAppServerProvider, CompletionRequest, McpEndpoint, Message, Provider,
    ToolDef,
};
use agentd_mcp::{bind_loopback, gen_token};
use agentd_permissions::Caller;
use agentd_types::{ActionCall, ActionResult, Dispatcher, RegistryError};
use async_trait::async_trait;

struct CallLog {
    calls: Mutex<Vec<ActionCall>>,
}
impl CallLog {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
        })
    }
    fn calls(&self) -> Vec<ActionCall> {
        self.calls.lock().unwrap().clone()
    }
}
#[async_trait]
impl Dispatcher for CallLog {
    async fn dispatch(
        &self,
        _caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
        self.calls.lock().unwrap().push(call);
        Ok((
            ActionResult {
                value: serde_json::json!({ "id": "evt_123", "ok": true }),
            },
            0,
        ))
    }
}

/// Resolve the provider/model/label from whichever gate env is set.
fn select() -> Option<(Arc<dyn Provider>, &'static str, &'static str)> {
    let on = |k: &str| std::env::var(k).ok().as_deref() == Some("1");
    if on("AGENTD_TEST_CLAUDE") {
        Some((
            Arc::new(ClaudeCliProvider::new()),
            "claude-haiku-4-5-20251001",
            "claude",
        ))
    } else if on("AGENTD_TEST_CODEX") {
        Some((Arc::new(CodexAppServerProvider::new()), "gpt-5.5", "codex"))
    } else {
        None
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_loop_fills_nested_input_schema() {
    let Some((provider, model, label)) = select() else {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1 or AGENTD_TEST_CODEX=1");
        return;
    };
    eprintln!("running nested-schema test against `{label}` ({model})");

    let log = CallLog::new();
    let dispatcher: Arc<dyn Dispatcher> = log.clone();

    // A nested schema mirroring compile_object_schema(strict=true): objects
    // within objects, an array of objects, required hoisted per level.
    let tools = vec![ToolDef {
        name: "calendar.create_event".into(),
        description: Some("Create a calendar event.".into()),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "minLength": 1 },
                "location": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "address": {
                            "type": "object",
                            "properties": {
                                "city": { "type": "string" },
                                "country": { "type": "string" }
                            },
                            "required": ["city"],
                            "additionalProperties": false
                        }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                },
                "attendees": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "email": { "type": "string" }
                        },
                        "required": ["name"],
                        "additionalProperties": false
                    }
                },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["location", "title"],
            "additionalProperties": false
        }),
    }];

    let token = provider.preferred_mcp_token().unwrap_or_else(gen_token);
    let loopback = bind_loopback(dispatcher, Caller::default(), tools.clone(), token)
        .await
        .expect("bind loopback");

    let req = CompletionRequest {
        system: Some(
            "You schedule calendar events. When asked, call the \
             `calendar.create_event` tool (MCP server `agentd`) exactly once, \
             filling every detail the user gives into the matching nested \
             fields. Do not ask clarifying questions."
                .into(),
        ),
        messages: vec![Message::user(
            "Create an event titled 'Team Offsite' at location named 'HQ' in \
             the city Lisbon, country Portugal. Attendees: Alice \
             (alice@example.com) and Bob (bob@example.com). Tag it 'work' and \
             'q3'.",
        )],
        model: Some(model.into()),
        max_tokens: Some(700),
        tools,
        mcp_endpoint: Some(McpEndpoint::Http {
            url: loopback.url.clone(),
            token: loopback.token.clone(),
        }),
        ..CompletionRequest::default()
    };

    let resp = provider.complete(req).await.expect("live CLI call w/ MCP");
    let calls = log.calls();
    assert!(
        !calls.is_empty(),
        "[{label}] expected calendar.create_event call, got 0\nresponse: {}",
        resp.text
    );
    let c = &calls[0];
    assert_eq!(c.action, "calendar.create_event", "[{label}] wrong tool: {c:?}");
    let a = &c.args;

    // Top level.
    assert!(
        a.get("title").and_then(|v| v.as_str()).is_some(),
        "[{label}] missing string `title`: {a}"
    );
    // One level deep.
    let loc = a.get("location").unwrap_or(&serde_json::Value::Null);
    assert!(
        loc.get("name").and_then(|v| v.as_str()).is_some(),
        "[{label}] missing `location.name`: {a}"
    );
    // Two levels deep.
    assert!(
        loc.get("address")
            .and_then(|ad| ad.get("city"))
            .and_then(|v| v.as_str())
            .is_some(),
        "[{label}] missing `location.address.city`: {a}"
    );
    // Array of objects — at least one attendee with a name.
    let attendees = a.get("attendees").and_then(|v| v.as_array());
    assert!(
        attendees
            .map(|arr| {
                !arr.is_empty()
                    && arr[0].get("name").and_then(|v| v.as_str()).is_some()
            })
            .unwrap_or(false),
        "[{label}] missing `attendees[].name`: {a}"
    );
    drop(loopback);
}

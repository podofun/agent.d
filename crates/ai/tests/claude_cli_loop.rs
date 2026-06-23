//! ClaudeCliProvider tool-use loop integration tests.
//!
//! These spin up a real `agentd-mcp` loopback against a stub
//! `Dispatcher`, point `ClaudeCliProvider` at it via `mcp_endpoint`, and
//! invoke the actual `claude` CLI. We assert two things:
//!
//! 1. the CLI returns a non-empty assistant message
//! 2. the stub dispatcher actually saw the expected tool call
//!
//! Gated behind `AGENTD_TEST_CLAUDE=1` because they cost tokens + need a
//! logged-in `claude` binary on PATH.
//!
//! Trade-off: we can't deterministically force a specific model output, so
//! we prompt the model with explicit "call tool X with args Y" instructions
//! and assert dispatch happened. If a future model variant ignores the
//! prompt and replies with text only, the test will fail loudly — that's a
//! signal worth investigating, not a flake.

use std::sync::{Arc, Mutex};

use agentd_ai::{ClaudeCliProvider, CompletionRequest, McpEndpoint, Message, Provider, ToolDef};
use agentd_mcp::bind_loopback;
use agentd_permissions::Caller;
use agentd_types::{ActionCall, ActionResult, Dispatcher, RegistryError};
use async_trait::async_trait;

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CLAUDE").ok().as_deref() == Some("1")
}

/// Stub Dispatcher that records every action call and returns `result_value`
/// (or a `Denied` error if the tool name matches `deny`). Wrapped in `Arc<>`
/// so the test holds a side reference for assertions while the MCP loopback
/// owns the dispatch path.
struct CallLog {
    calls: Mutex<Vec<ActionCall>>,
    result_value: serde_json::Value,
    deny: Option<String>,
}

impl CallLog {
    fn new(value: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            result_value: value,
            deny: None,
        })
    }
    fn with_deny(value: serde_json::Value, deny_tool: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            result_value: value,
            deny: Some(deny_tool.into()),
        })
    }
    fn into_dispatcher(self: Arc<Self>) -> Arc<dyn Dispatcher> {
        self
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
        self.calls.lock().unwrap().push(call.clone());
        if let Some(deny) = &self.deny
            && &call.action == deny
        {
            return Err((
                RegistryError::Denied {
                    layer: "Runner".into(),
                    reason: format!("runner not allowed to call `{}`", call.action),
                },
                0,
            ));
        }
        Ok((
            ActionResult {
                value: self.result_value.clone(),
            },
            0,
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_loop_calls_tool_via_mcp() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1");
        return;
    }

    // The dispatcher returns a structured note. We instruct claude to call
    // `notes.lookup` and quote the `found` field back to us so we can
    // anchor an assertion on the final text too.
    let log = CallLog::new(serde_json::json!({ "found": "the agentd answer is 42" }));
    let dispatcher = log.clone().into_dispatcher();
    let tools = vec![ToolDef {
        name: "notes.lookup".into(),
        description: Some(
            "Look up the agentd answer. Returns an object with a `found` field.".into(),
        ),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": { "q": { "type": "string" } },
            "required": ["q"],
        }),
    }];
    let loopback = bind_loopback(
        dispatcher,
        Caller::default(),
        tools.clone(),
        agentd_mcp::gen_token(),
    )
    .await
    .expect("bind loopback");

    let p = ClaudeCliProvider::new()
        // Use Haiku for speed/cost; Opus would be overkill for a tool call.
        .with_extra_args::<_, &str>(std::iter::empty::<&str>());
    let req = CompletionRequest {
        system: Some(
            "When the user asks for the agentd answer, you MUST call the \
             `notes.lookup` tool with arguments {\"q\":\"answer\"} exactly \
             once, then quote the value of the `found` field verbatim in \
             your reply."
                .into(),
        ),
        messages: vec![Message::user(
            "Look up the agentd answer using your tool and tell me what it says.",
        )],
        prompt: None,
        model: Some("claude-haiku-4-5-20251001".into()),
        max_tokens: Some(512),
        tools,
        mcp_endpoint: Some(McpEndpoint::Http {
            url: loopback.url.clone(),
            token: loopback.token.clone(),
        }),
        ..CompletionRequest::default()
    };

    let resp = p.complete(req).await.expect("live claude call w/ MCP");
    let calls = log.calls();

    assert!(
        !calls.is_empty(),
        "expected claude to invoke notes.lookup via MCP, got 0 calls\n\
         response was: {}",
        resp.text
    );
    let first = &calls[0];
    assert_eq!(first.action, "notes.lookup", "unexpected tool: {first:?}");
    // The CLI propagates the reply back. We don't insist on exact wording
    // — model variation is real — but the substring from the tool result
    // must appear so we know the loop actually closed.
    assert!(
        resp.text.to_lowercase().contains("42"),
        "expected `42` from tool result to appear in reply, got: {}",
        resp.text
    );
    assert!(
        resp.tool_calls.is_empty(),
        "CLI provider must not surface tool_calls"
    );
    drop(loopback);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_loop_fills_declared_input_schema_fields() {
    // A declared `input` schema must reach the model as the tool's
    // input_schema and shape the call — the model fills the schema's
    // named, required fields rather than guessing.
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1");
        return;
    }

    let log = CallLog::new(serde_json::json!({ "status": 200 }));
    let dispatcher = log.clone().into_dispatcher();
    let tools = vec![ToolDef {
        name: "discord.send".into(),
        description: Some("Send a message to a Discord channel.".into()),
        // Mirrors compile_object_schema(discord.send.input, strict=true).
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "channel_id": { "type": "string", "description": "Discord channel snowflake" },
                "content": { "type": "string", "minLength": 1, "description": "Message body (<=2000 chars)" },
            },
            "required": ["channel_id", "content"],
            "additionalProperties": false,
        }),
    }];
    let loopback = bind_loopback(
        dispatcher,
        Caller::default(),
        tools.clone(),
        agentd_mcp::gen_token(),
    )
    .await
    .expect("bind loopback");

    let p = ClaudeCliProvider::new();
    let req = CompletionRequest {
        system: Some(
            "You send Discord messages. When asked, call the `discord.send` \
             tool exactly once with the channel id and message text. Do not \
             ask clarifying questions."
                .into(),
        ),
        messages: vec![Message::user(
            "Send the message 'Hello from agentd' to Discord channel 123456789012345678.",
        )],
        prompt: None,
        model: Some("claude-haiku-4-5-20251001".into()),
        max_tokens: Some(512),
        tools,
        mcp_endpoint: Some(McpEndpoint::Http {
            url: loopback.url.clone(),
            token: loopback.token.clone(),
        }),
        ..CompletionRequest::default()
    };

    let resp = p.complete(req).await.expect("live claude call w/ MCP");
    let calls = log.calls();
    assert!(
        !calls.is_empty(),
        "expected claude to invoke discord.send, got 0 calls\nresponse was: {}",
        resp.text
    );
    let args = &calls[0].args;
    assert_eq!(calls[0].action, "discord.send");
    assert!(
        args.get("channel_id").and_then(|v| v.as_str()).is_some(),
        "model didn't fill schema field `channel_id`: {args}"
    );
    assert!(
        args.get("content").and_then(|v| v.as_str()).is_some(),
        "model didn't fill schema field `content`: {args}"
    );
    drop(loopback);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_loop_handles_denied_tool() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1");
        return;
    }

    // Dispatcher denies the only available tool. The CLI should see the
    // tool_result error, give up, and produce a text reply explaining it
    // couldn't get the value. We don't pin the wording — we just confirm
    // the call was attempted + the final text isn't empty.
    let log = CallLog::with_deny(
        serde_json::json!({ "found": "unreachable" }),
        "notes.lookup",
    );
    let dispatcher = log.clone().into_dispatcher();
    let tools = vec![ToolDef {
        name: "notes.lookup".into(),
        description: Some("Look up a note.".into()),
        input_schema: serde_json::json!({ "type": "object" }),
    }];
    let loopback = bind_loopback(
        dispatcher,
        Caller::default(),
        tools.clone(),
        agentd_mcp::gen_token(),
    )
    .await
    .expect("bind loopback");

    let p = ClaudeCliProvider::new();
    let req = CompletionRequest {
        system: Some(
            "When asked for the agentd answer, you MUST call `notes.lookup` \
             with {\"q\":\"answer\"}. If the tool returns an error, just say \
             so plainly and stop."
                .into(),
        ),
        messages: vec![Message::user("Look up the agentd answer with your tool.")],
        prompt: None,
        model: Some("claude-haiku-4-5-20251001".into()),
        max_tokens: Some(256),
        tools,
        mcp_endpoint: Some(McpEndpoint::Http {
            url: loopback.url.clone(),
            token: loopback.token.clone(),
        }),
        ..CompletionRequest::default()
    };

    let resp = p.complete(req).await.expect("live claude call w/ MCP");
    let calls = log.calls();
    assert!(!calls.is_empty(), "expected tool dispatch attempt");
    assert!(!resp.text.is_empty(), "expected non-empty fallback reply");
    drop(loopback);
}

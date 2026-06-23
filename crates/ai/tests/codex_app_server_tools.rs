//! Live tool-use test: real `codex app-server` driving a dynamic tool
//! through agentd's MCP loopback. The codex side talks the streamable-
//! HTTP MCP transport against `agentd-mcp::bind_loopback`; tool calls
//! flow back through the stub `Dispatcher` so we can assert dispatch +
//! reply propagation without standing up a Lua host.
//!
//! Gated `AGENTD_TEST_CODEX=1`. Costs tokens; needs logged-in `codex`.

use std::sync::{Arc, Mutex};

use agentd_ai::{
    CodexAppServerProvider, CompletionRequest, McpEndpoint, Message, Provider, ToolDef,
};
use agentd_mcp::bind_loopback;
use agentd_permissions::Caller;
use agentd_types::{ActionCall, ActionResult, Dispatcher, RegistryError};
use async_trait::async_trait;

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CODEX").ok().as_deref() == Some("1")
}

struct CallLog {
    calls: Mutex<Vec<ActionCall>>,
    result_value: serde_json::Value,
}

impl CallLog {
    fn new(value: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            result_value: value,
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
        Ok((
            ActionResult {
                value: self.result_value.clone(),
            },
            0,
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_loop_calls_dynamic_tool_via_mcp() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CODEX=1");
        return;
    }

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
    // Mirror the executor's token coordination: codex bakes its bearer into
    // the subprocess env at spawn, so the loopback must be bound with the
    // provider's own token (not a fresh random one) or codex's calls 401.
    let p = CodexAppServerProvider::new();
    let token = p.preferred_mcp_token().expect("codex provides a token");
    let loopback = bind_loopback(dispatcher, Caller::default(), tools.clone(), token)
        .await
        .expect("bind loopback");

    let req = CompletionRequest {
        system: Some(
            "When the user asks for the agentd answer, you MUST call the \
             `notes.lookup` MCP tool (server `agentd`) with arguments \
             {\"q\":\"answer\"} exactly once, then quote the value of the \
             `found` field verbatim in your reply."
                .into(),
        ),
        messages: vec![Message::user(
            "Look up the agentd answer using your tool and tell me what it says.",
        )],
        model: Some("gpt-5.5".into()),
        max_tokens: Some(512),
        tools,
        mcp_endpoint: Some(McpEndpoint::Http {
            url: loopback.url.clone(),
            token: loopback.token.clone(),
        }),
        ..CompletionRequest::default()
    };

    let resp = p.complete(req).await.expect("live codex call w/ MCP");
    let calls = log.calls();

    assert!(
        !calls.is_empty(),
        "expected codex to invoke notes.lookup via MCP, got 0 calls\n\
         response was: {}",
        resp.text
    );
    let first = &calls[0];
    assert_eq!(first.action, "notes.lookup", "unexpected tool: {first:?}");
    assert!(
        resp.text.to_lowercase().contains("42"),
        "expected `42` from tool result to appear in reply, got: {}",
        resp.text
    );
    assert!(
        resp.tool_calls.is_empty(),
        "provider must not surface tool_calls"
    );
    drop(loopback);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_loop_fills_declared_input_schema_fields() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CODEX=1");
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
    let p = CodexAppServerProvider::new();
    let token = p.preferred_mcp_token().expect("codex provides a token");
    let loopback = bind_loopback(dispatcher, Caller::default(), tools.clone(), token)
        .await
        .expect("bind loopback");

    let req = CompletionRequest {
        system: Some(
            "You send Discord messages. When asked, call the `discord.send` \
             MCP tool (server `agentd`) exactly once with the channel id and \
             message text. Do not ask clarifying questions."
                .into(),
        ),
        messages: vec![Message::user(
            "Send the message 'Hello from agentd' to Discord channel 123456789012345678.",
        )],
        model: Some("gpt-5.5".into()),
        max_tokens: Some(512),
        tools,
        mcp_endpoint: Some(McpEndpoint::Http {
            url: loopback.url.clone(),
            token: loopback.token.clone(),
        }),
        ..CompletionRequest::default()
    };

    let resp = p.complete(req).await.expect("live codex call w/ MCP");
    let calls = log.calls();
    assert!(
        !calls.is_empty(),
        "expected codex to invoke discord.send, got 0 calls\nresponse was: {}",
        resp.text
    );
    let args = &calls[0].args;
    assert_eq!(calls[0].action, "discord.send", "unexpected tool: {:?}", calls[0]);
    assert!(
        args.get("channel_id").and_then(|v| v.as_str()).is_some(),
        "codex didn't fill schema field `channel_id`: {args}"
    );
    assert!(
        args.get("content").and_then(|v| v.as_str()).is_some(),
        "codex didn't fill schema field `content`: {args}"
    );
    drop(loopback);
}

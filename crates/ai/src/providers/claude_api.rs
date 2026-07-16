use std::sync::Arc;

use agentd_net::http::{Request as HttpRequest, send as http_send};
use agentd_secrets::SecretStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{
    CompletionRequest, CompletionResponse, LoopMode, Message, Provider, ProviderError, Role,
    ToolCall,
};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MODEL: &str = "claude-opus-4-7";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const SECRET_KEY: &str = "anthropic_api_key";

pub struct ClaudeApiProvider {
    name: String,
    secrets: Arc<dyn SecretStore>,
    /// Override endpoint (tests use a local mock server; config-driven
    /// providers point it at a compatible gateway). Falls back to
    /// `ANTHROPIC_URL` when `None`.
    endpoint: Option<String>,
    /// Secret key name for the API key. `None` disables the `x-api-key`
    /// header entirely (unauthenticated local proxies).
    secret_key: Option<String>,
    /// Model used when the request carries none. Per-instance so compatible
    /// gateways can pick their own default.
    default_model: String,
}

impl ClaudeApiProvider {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            name: "anthropic".into(),
            secrets,
            endpoint: None,
            secret_key: Some(SECRET_KEY.into()),
            default_model: DEFAULT_MODEL.into(),
        }
    }

    /// Registry name for a config-driven instance (e.g. `"my-gateway"`).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn with_secret_key(mut self, key: impl Into<String>) -> Self {
        self.secret_key = Some(key.into());
        self
    }

    /// Send no `x-api-key` header at all (unauthenticated local proxies).
    pub fn with_no_auth(mut self) -> Self {
        self.secret_key = None;
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(ANTHROPIC_URL)
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn api_key(&self) -> Result<Option<String>, ProviderError> {
        match &self.secret_key {
            None => Ok(None),
            Some(k) => self
                .secrets
                .get(k)
                .map(Some)
                .map_err(|e| ProviderError::Config(format!("read `{k}`: {e}"))),
        }
    }
}

#[async_trait]
impl Provider for ClaudeApiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn loop_mode(&self) -> LoopMode {
        LoopMode::ExecutorOwned
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let api_key = self.api_key()?;

        let body = build_request_body(&req, self.default_model());

        let mut http_req = HttpRequest {
            method: "POST".into(),
            url: self.endpoint().to_string(),
            json: Some(body),
            ..Default::default()
        };
        if let Some(key) = api_key {
            http_req.headers.insert("x-api-key".into(), key);
        }
        http_req
            .headers
            .insert("anthropic-version".into(), ANTHROPIC_VERSION.into());

        let resp = http_send(http_req)
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !(200..300).contains(&resp.status) {
            return Err(ProviderError::Upstream(format!(
                "anthropic {}: {}",
                resp.status, resp.body
            )));
        }

        let parsed: MessagesResponse = serde_json::from_str(&resp.body).map_err(|e| {
            ProviderError::Upstream(format!(
                "decode anthropic response: {e}\nbody: {}",
                resp.body
            ))
        })?;

        Ok(translate_response(parsed, req.model))
    }
}

fn build_request_body(req: &CompletionRequest, default_model: &str) -> serde_json::Value {
    let model = req.model.clone().unwrap_or_else(|| default_model.into());
    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let mut messages_out: Vec<serde_json::Value> = Vec::new();
    // The "prompt" convenience field becomes a trailing user message if no
    // explicit messages were provided; otherwise it's appended after them.
    let mut messages = req.messages.clone();
    if let Some(p) = &req.prompt
        && !p.is_empty()
    {
        messages.push(Message::user(p.clone()));
    }
    for m in messages {
        let role = match m.role {
            // Anthropic's wire format only has `user` and `assistant`. Tool
            // results travel inside a user-role message with a tool_result
            // content block.
            Role::System | Role::User | Role::Tool => "user",
            Role::Assistant => "assistant",
        };
        // Build content blocks. Order: text (if any) → tool_use blocks for
        // assistant turns that called tools, or tool_result for Role::Tool.
        let mut blocks: Vec<serde_json::Value> = Vec::new();
        match m.role {
            Role::Tool => {
                blocks.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                }));
            }
            _ => {
                if !m.content.is_empty() {
                    blocks.push(serde_json::json!({ "type": "text", "text": m.content }));
                }
                for tc in &m.tool_calls {
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                    }));
                }
            }
        }
        // Skip empty messages — Anthropic rejects them.
        if blocks.is_empty() {
            continue;
        }
        messages_out.push(serde_json::json!({ "role": role, "content": blocks }));
    }

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages_out,
    });
    if let Some(sys) = &req.system {
        body["system"] = serde_json::Value::String(sys.clone());
    }
    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description.clone().unwrap_or_default(),
                    "input_schema": if t.input_schema.is_null() {
                        serde_json::json!({ "type": "object" })
                    } else {
                        t.input_schema.clone()
                    },
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
    }
    body
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

fn translate_response(
    parsed: MessagesResponse,
    requested_model: Option<String>,
) -> CompletionResponse {
    let mut text = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for block in parsed.content {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t);
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                });
            }
            ContentBlock::Other => {}
        }
    }
    CompletionResponse {
        text,
        model: parsed.model.or(requested_model),
        stop_reason: parsed.stop_reason,
        tool_calls,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDef;
    use agentd_secrets::MemoryStore;

    #[test]
    fn request_body_includes_system_tools_and_tool_result() {
        let secrets = Arc::new(MemoryStore::default());
        secrets.set("anthropic_api_key", "k").unwrap();
        let _p = ClaudeApiProvider::new(secrets);

        let req = CompletionRequest {
            system: Some("be terse".into()),
            messages: vec![
                Message::user("hello"),
                Message {
                    role: Role::Assistant,
                    content: "let me look that up".into(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "notes.lookup".into(),
                        arguments: serde_json::json!({ "q": "x" }),
                    }],
                    tool_call_id: None,
                },
                Message::tool_result("c1", r#"{"found":"x=42"}"#),
            ],
            tools: vec![ToolDef {
                name: "notes.lookup".into(),
                description: Some("read note".into()),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            model: Some("claude-foo".into()),
            ..Default::default()
        };
        let body = build_request_body(&req, DEFAULT_MODEL);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["model"], "claude-foo");
        assert_eq!(body["tools"][0]["name"], "notes.lookup");
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        // Assistant turn carries the tool_use block.
        assert_eq!(msgs[1]["role"], "assistant");
        let assist_blocks = msgs[1]["content"].as_array().unwrap();
        assert!(assist_blocks.iter().any(|b| b["type"] == "tool_use"));
        // Tool result message lives under role=user w/ tool_result block.
        assert_eq!(msgs[2]["role"], "user");
        let tr = &msgs[2]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["tool_use_id"], "c1");
    }

    #[test]
    fn builder_overrides_name_and_default_model() {
        let secrets = Arc::new(MemoryStore::default());
        let p = ClaudeApiProvider::new(secrets)
            .with_name("my-gateway")
            .with_default_model("claude-custom");
        assert_eq!(p.name(), "my-gateway");
        let body = build_request_body(&CompletionRequest::default(), p.default_model());
        assert_eq!(body["model"], "claude-custom");
    }

    #[test]
    fn no_auth_mode_needs_no_secret() {
        let secrets = Arc::new(MemoryStore::default());
        let p = ClaudeApiProvider::new(secrets).with_no_auth();
        assert!(p.api_key().unwrap().is_none());
    }

    #[test]
    fn missing_secret_is_a_config_error_when_auth_required() {
        let secrets = Arc::new(MemoryStore::default());
        let p = ClaudeApiProvider::new(secrets);
        assert!(matches!(p.api_key(), Err(ProviderError::Config(_))));
    }

    #[test]
    fn translates_text_and_tool_use_blocks() {
        let raw = serde_json::json!({
            "model": "claude-opus-4-7",
            "stop_reason": "tool_use",
            "content": [
                { "type": "text", "text": "let me check" },
                { "type": "tool_use", "id": "c1", "name": "notes.lookup", "input": { "q": "x" } },
            ],
        });
        let parsed: MessagesResponse = serde_json::from_value(raw).unwrap();
        let r = translate_response(parsed, None);
        assert_eq!(r.text, "let me check");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "notes.lookup");
    }
}

use std::sync::Arc;

use agentd_net::http::{Request as HttpRequest, send as http_send, send_streaming};
use agentd_secrets::SecretStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::sse::SseBuffer;
use crate::types::{
    CompletionRequest, CompletionResponse, LoopMode, Message, Provider, ProviderError, Role,
    StreamEvent, StreamSink, ToolCall,
};

const OPENAI_URL: &str = "https://api.openai.com/v1/chat/completions";
const DEFAULT_MODEL: &str = "gpt-4.1";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const SECRET_KEY: &str = "openai_api_key";

/// Direct OpenAI Chat Completions provider. Key path — the subscription/CLI
/// paths live in `CodexAppServerProvider` (`codex`) and `CodexCliProvider`
/// (`openai-cli`). `ExecutorOwned`: returns `tool_calls`, executor drives the
/// loop. Mirrors `ClaudeApiProvider` modulo the OpenAI wire format.
pub struct OpenAiApiProvider {
    name: String,
    secrets: Arc<dyn SecretStore>,
    /// Override endpoint (tests point this at a local mock server; config-driven
    /// providers point it at a compatible host). Falls back to `OPENAI_URL`
    /// when `None`.
    endpoint: Option<String>,
    /// Secret key name for the API key. `None` disables the `Authorization`
    /// header entirely (local servers such as Ollama/vLLM without auth).
    secret_key: Option<String>,
    /// Model used when the request carries none. Per-instance so compatible
    /// vendors get a sensible default (`gpt-4.1` means nothing to OpenRouter).
    default_model: String,
}

impl OpenAiApiProvider {
    pub fn new(secrets: Arc<dyn SecretStore>) -> Self {
        Self {
            name: "openai".into(),
            secrets,
            endpoint: None,
            secret_key: Some(SECRET_KEY.into()),
            default_model: DEFAULT_MODEL.into(),
        }
    }

    /// Registry name for a config-driven instance (e.g. `"openrouter"`).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(normalize_openai_endpoint(&endpoint.into()));
        self
    }

    pub fn with_secret_key(mut self, key: impl Into<String>) -> Self {
        self.secret_key = Some(key.into());
        self
    }

    /// Send no `Authorization` header at all (unauthenticated local servers).
    pub fn with_no_auth(mut self) -> Self {
        self.secret_key = None;
        self
    }

    pub fn with_default_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = model.into();
        self
    }

    fn endpoint(&self) -> &str {
        self.endpoint.as_deref().unwrap_or(OPENAI_URL)
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn api_key(&self) -> Result<Option<String>, ProviderError> {
        match &self.secret_key {
            None => Ok(None),
            Some(k) => self.secrets.get(k).map(Some).map_err(|e| match e {
                agentd_secrets::SecretError::NotFound(_) => ProviderError::Config(format!(
                    "provider `{}` is not configured — store the API key in the `{k}` secret to use it",
                    self.name
                )),
                e => ProviderError::Config(format!(
                    "provider `{}` could not read the `{k}` secret ({e})",
                    self.name
                )),
            }),
        }
    }
}

#[async_trait]
impl Provider for OpenAiApiProvider {
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
            http_req
                .headers
                .insert("authorization".into(), format!("Bearer {key}"));
        }

        let resp = http_send(http_req)
            .await
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !(200..300).contains(&resp.status) {
            return Err(ProviderError::Upstream(format!(
                "openai {}: {}",
                resp.status, resp.body
            )));
        }

        let parsed: ChatResponse = serde_json::from_str(&resp.body).map_err(|e| {
            ProviderError::Upstream(format!("decode openai response: {e}\nbody: {}", resp.body))
        })?;

        translate_response(parsed, req.model)
    }

    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        sink: StreamSink,
    ) -> Result<CompletionResponse, ProviderError> {
        let api_key = self.api_key()?;

        let mut body = build_request_body(&req, self.default_model());
        body["stream"] = serde_json::Value::Bool(true);

        let mut http_req = HttpRequest {
            method: "POST".into(),
            url: self.endpoint().to_string(),
            json: Some(body),
            // Streamed turns outlive the 30s default while staying bounded.
            timeout_ms: Some(600_000),
            ..Default::default()
        };
        if let Some(key) = api_key {
            http_req
                .headers
                .insert("authorization".into(), format!("Bearer {key}"));
        }

        let mut acc = ChunkAccumulator::default();
        let mut sse = SseBuffer::new();
        let resp = send_streaming(http_req, |chunk| {
            sse.push(chunk, |data| acc.feed(data, &sink));
        })
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if !(200..300).contains(&resp.status) {
            return Err(ProviderError::Upstream(format!(
                "openai {}: {}",
                resp.status, resp.body
            )));
        }
        acc.finish(req.model)
    }
}

/// Rebuilds the complete response from OpenAI's chunked SSE stream while
/// relaying text deltas and tool-call announcements into the sink.
#[derive(Default)]
struct ChunkAccumulator {
    model: Option<String>,
    finish_reason: Option<String>,
    text: String,
    /// Tool calls by chunk index; arguments accumulate as string fragments.
    tool_calls: std::collections::BTreeMap<u64, (String, String, String)>,
    saw_chunk: bool,
}

impl ChunkAccumulator {
    fn feed(&mut self, data: &str, sink: &StreamSink) {
        if data.trim() == "[DONE]" {
            let _ = sink.send(StreamEvent::TurnEnd);
            return;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };
        self.saw_chunk = true;
        if let Some(m) = ev["model"].as_str() {
            self.model = Some(m.to_string());
        }
        let Some(choice) = ev["choices"].get(0) else {
            return;
        };
        if let Some(f) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(f.to_string());
        }
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str()
            && !t.is_empty()
        {
            let _ = sink.send(StreamEvent::TextDelta {
                text: t.to_string(),
            });
            self.text.push_str(t);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for c in calls {
                let idx = c["index"].as_u64().unwrap_or(0);
                let entry = self
                    .tool_calls
                    .entry(idx)
                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                if let Some(id) = c["id"].as_str() {
                    entry.0.push_str(id);
                }
                if let Some(n) = c["function"]["name"].as_str() {
                    entry.1.push_str(n);
                    let _ = sink.send(StreamEvent::ToolCall {
                        name: entry.1.clone(),
                    });
                }
                if let Some(a) = c["function"]["arguments"].as_str() {
                    entry.2.push_str(a);
                }
            }
        }
    }

    fn finish(self, requested_model: Option<String>) -> Result<CompletionResponse, ProviderError> {
        if !self.saw_chunk {
            return Err(ProviderError::EmptyResponse);
        }
        let tool_calls = self
            .tool_calls
            .into_values()
            .map(|(id, name, args)| {
                let arguments = if args.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&args).unwrap_or(serde_json::Value::String(args))
                };
                ToolCall {
                    id,
                    name,
                    arguments,
                }
            })
            .collect();
        Ok(CompletionResponse {
            text: self.text,
            model: self.model.or(requested_model),
            stop_reason: self.finish_reason.map(normalize_stop_reason),
            tool_calls,
        })
    }
}

/// Accepts a base URL (`.../v1`), with or without trailing slash, or a full
/// chat-completions URL, and returns the full endpoint. Forgiving on purpose:
/// users paste whatever their vendor's docs show.
pub fn normalize_openai_endpoint(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/chat/completions")
    }
}

fn build_request_body(req: &CompletionRequest, default_model: &str) -> serde_json::Value {
    let model = req.model.clone().unwrap_or_else(|| default_model.into());
    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let mut messages_out: Vec<serde_json::Value> = Vec::new();
    // System instruction rides as a leading `system` message in OpenAI's format.
    if let Some(sys) = &req.system
        && !sys.is_empty()
    {
        messages_out.push(serde_json::json!({ "role": "system", "content": sys }));
    }

    // The "prompt" convenience field becomes a trailing user message.
    let mut messages = req.messages.clone();
    if let Some(p) = &req.prompt
        && !p.is_empty()
    {
        messages.push(Message::user(p.clone()));
    }
    for m in messages {
        match m.role {
            Role::Tool => {
                // OpenAI tool results are their own role with a tool_call_id.
                messages_out.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": m.tool_call_id.clone().unwrap_or_default(),
                    "content": m.content,
                }));
            }
            Role::Assistant => {
                let mut msg = serde_json::json!({ "role": "assistant" });
                // OpenAI requires content present (null OK) on assistant turns.
                msg["content"] = if m.content.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(m.content.clone())
                };
                if !m.tool_calls.is_empty() {
                    let calls: Vec<serde_json::Value> = m
                        .tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    // OpenAI wants arguments as a JSON *string*.
                                    "arguments": serde_json::to_string(&tc.arguments)
                                        .unwrap_or_else(|_| "{}".into()),
                                },
                            })
                        })
                        .collect();
                    msg["tool_calls"] = serde_json::Value::Array(calls);
                }
                messages_out.push(msg);
            }
            // System messages mid-conversation + plain user turns.
            Role::System => {
                if !m.content.is_empty() {
                    messages_out
                        .push(serde_json::json!({ "role": "system", "content": m.content }));
                }
            }
            Role::User => {
                messages_out.push(serde_json::json!({ "role": "user", "content": m.content }));
            }
        }
    }

    let mut body = serde_json::json!({
        "model": model,
        "max_completion_tokens": max_tokens,
        "messages": messages_out,
    });
    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description.clone().unwrap_or_default(),
                        "parameters": if t.input_schema.is_null() {
                            serde_json::json!({ "type": "object" })
                        } else {
                            t.input_schema.clone()
                        },
                    },
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
    }
    body
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Choice {
    #[serde(default)]
    finish_reason: Option<String>,
    message: ChoiceMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCallWire {
    #[serde(default)]
    id: String,
    function: FunctionWire,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FunctionWire {
    name: String,
    /// OpenAI returns arguments as a JSON-encoded string.
    #[serde(default)]
    arguments: String,
}

fn translate_response(
    parsed: ChatResponse,
    requested_model: Option<String>,
) -> Result<CompletionResponse, ProviderError> {
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or(ProviderError::EmptyResponse)?;

    let text = choice.message.content.unwrap_or_default();
    let tool_calls: Vec<ToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tc| {
            // Empty string → empty object; malformed → wrap verbatim so the
            // tool handler can surface the parse error rather than us eating it.
            let arguments = if tc.function.arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&tc.function.arguments)
                    .unwrap_or(serde_json::Value::String(tc.function.arguments.clone()))
            };
            ToolCall {
                id: tc.id,
                name: tc.function.name,
                arguments,
            }
        })
        .collect();

    Ok(CompletionResponse {
        text,
        model: parsed.model.or(requested_model),
        stop_reason: choice.finish_reason.map(normalize_stop_reason),
        tool_calls,
    })
}

/// Map OpenAI `finish_reason` vocabulary onto the canonical (Anthropic-style)
/// `stop_reason` values so wire-format differences never leak past the
/// `Provider` trait — Lua sees identical strings whatever the provider.
fn normalize_stop_reason(finish_reason: String) -> String {
    match finish_reason.as_str() {
        "stop" => "end_turn".into(),
        "tool_calls" | "function_call" => "tool_use".into(),
        "length" => "max_tokens".into(),
        _ => finish_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolDef;
    use agentd_secrets::MemoryStore;

    #[test]
    fn normalize_endpoint_appends_chat_completions() {
        assert_eq!(
            normalize_openai_endpoint("https://openrouter.ai/api/v1"),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            normalize_openai_endpoint("http://localhost:11434/v1/"),
            "http://localhost:11434/v1/chat/completions"
        );
        // already-full URL passes through untouched
        assert_eq!(
            normalize_openai_endpoint("https://api.groq.com/openai/v1/chat/completions"),
            "https://api.groq.com/openai/v1/chat/completions"
        );
    }

    #[test]
    fn with_endpoint_normalizes() {
        let secrets = Arc::new(MemoryStore::default());
        let p = OpenAiApiProvider::new(secrets).with_endpoint("http://localhost:11434/v1");
        assert_eq!(p.endpoint(), "http://localhost:11434/v1/chat/completions");
    }

    #[test]
    fn builder_overrides_name_and_default_model() {
        let secrets = Arc::new(MemoryStore::default());
        let p = OpenAiApiProvider::new(secrets)
            .with_name("openrouter")
            .with_default_model("meta-llama/llama-3.3-70b-instruct");
        assert_eq!(p.name(), "openrouter");
        // default model lands in the body when the request has none
        let body = build_request_body(&CompletionRequest::default(), p.default_model());
        assert_eq!(body["model"], "meta-llama/llama-3.3-70b-instruct");
    }

    #[test]
    fn no_auth_mode_needs_no_secret() {
        // MemoryStore is empty — with_no_auth() must not try to read a key.
        let secrets = Arc::new(MemoryStore::default());
        let p = OpenAiApiProvider::new(secrets).with_no_auth();
        assert!(p.api_key().unwrap().is_none());
    }

    #[test]
    fn missing_secret_is_a_config_error_when_auth_required() {
        let secrets = Arc::new(MemoryStore::default());
        let p = OpenAiApiProvider::new(secrets);
        assert!(matches!(p.api_key(), Err(ProviderError::Config(_))));
    }

    #[test]
    fn request_body_includes_system_tools_and_tool_result() {
        let secrets = Arc::new(MemoryStore::default());
        secrets.set("openai_api_key", "k").unwrap();
        let _p = OpenAiApiProvider::new(secrets);

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
            model: Some("gpt-foo".into()),
            ..Default::default()
        };
        let body = build_request_body(&req, DEFAULT_MODEL);
        assert_eq!(body["model"], "gpt-foo");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "notes.lookup");
        let msgs = body["messages"].as_array().unwrap();
        // system + user + assistant + tool
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be terse");
        assert_eq!(msgs[2]["role"], "assistant");
        let calls = msgs[2]["tool_calls"].as_array().unwrap();
        assert_eq!(calls[0]["id"], "c1");
        assert_eq!(calls[0]["function"]["name"], "notes.lookup");
        // Arguments serialized as a JSON string, not an object.
        assert!(calls[0]["function"]["arguments"].is_string());
        // Tool result is its own role w/ tool_call_id.
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
    }

    #[test]
    fn translates_text_and_tool_calls() {
        let raw = serde_json::json!({
            "model": "gpt-4.1",
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "content": "let me check",
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": { "name": "notes.lookup", "arguments": "{\"q\":\"x\"}" },
                    }],
                },
            }],
        });
        let parsed: ChatResponse = serde_json::from_value(raw).unwrap();
        let r = translate_response(parsed, None).unwrap();
        assert_eq!(r.text, "let me check");
        assert_eq!(r.tool_calls.len(), 1);
        assert_eq!(r.tool_calls[0].name, "notes.lookup");
        assert_eq!(r.tool_calls[0].arguments["q"], "x");
        // Canonical vocabulary, not the OpenAI wire value ("tool_calls").
        assert_eq!(r.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn empty_choices_is_error() {
        let parsed: ChatResponse =
            serde_json::from_value(serde_json::json!({ "choices": [] })).unwrap();
        assert!(matches!(
            translate_response(parsed, None),
            Err(ProviderError::EmptyResponse)
        ));
    }

    #[test]
    fn chunk_accumulator_rebuilds_full_response_and_emits_deltas() {
        use crate::types::StreamEvent;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut acc = ChunkAccumulator::default();
        for ev in [
            r#"{"model":"gpt-x","choices":[{"delta":{"content":"Hel"}}]}"#,
            r#"{"choices":[{"delta":{"content":"lo"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"notes.lookup"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"q\":\"x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ] {
            acc.feed(ev, &tx);
        }
        let resp = acc.finish(None).unwrap();
        assert_eq!(resp.text, "Hello");
        assert_eq!(resp.model.as_deref(), Some("gpt-x"));
        // Canonical vocabulary, same normalization as the aggregate path.
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(resp.tool_calls.len(), 1);
        assert_eq!(resp.tool_calls[0].id, "c1");
        assert_eq!(resp.tool_calls[0].arguments, serde_json::json!({"q":"x"}));

        let mut deltas = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            deltas.push(ev);
        }
        assert!(matches!(&deltas[0], StreamEvent::TextDelta { text } if text == "Hel"));
        assert!(matches!(&deltas[1], StreamEvent::TextDelta { text } if text == "lo"));
        assert!(matches!(&deltas[2], StreamEvent::ToolCall { name } if name == "notes.lookup"));
        assert!(matches!(deltas.last(), Some(StreamEvent::TurnEnd)));
    }

    #[test]
    fn chunk_accumulator_with_no_chunks_is_empty_response() {
        let acc = ChunkAccumulator::default();
        assert!(matches!(
            acc.finish(None),
            Err(ProviderError::EmptyResponse)
        ));
    }
}

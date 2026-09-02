use std::sync::Mutex;

use async_trait::async_trait;

use crate::types::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, StreamEvent, StreamSink,
    ToolCall,
};

/// Deterministic provider for tests. Two modes:
///
/// - **Default / `with_reply`**: returns canned text (or echoes the flattened
///   prompt). No tool calls.
/// - **`with_script`**: returns a pre-recorded sequence of responses, one per
///   `complete()` call. Lets tests rehearse multi-turn flows including
///   `tool_calls` emission, then a final plain reply.
pub struct MockProvider {
    name: String,
    reply: Option<String>,
    script: Mutex<Option<Vec<CompletionResponse>>>,
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            name: "mock".into(),
            reply: None,
            script: Mutex::new(None),
        }
    }
    pub fn with_reply(mut self, reply: impl Into<String>) -> Self {
        self.reply = Some(reply.into());
        self
    }
    /// Queue up a fixed sequence of responses. Each `complete()` call
    /// consumes the next one. After the script is exhausted, the provider
    /// errors out (Upstream) so tests catch unexpected extra turns.
    pub fn with_script(self, responses: Vec<CompletionResponse>) -> Self {
        *self.script.lock().unwrap() = Some(responses);
        self
    }
    /// Convenience: build a one-shot tool-call response that asks the
    /// executor to run `name(arguments)`. Tests chain `tool_call` then
    /// `text_only` to rehearse the full loop.
    pub fn tool_call(
        call_id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> CompletionResponse {
        CompletionResponse {
            text: String::new(),
            model: None,
            stop_reason: Some("tool_use".into()),
            tool_calls: vec![ToolCall {
                id: call_id.into(),
                name: name.into(),
                arguments,
            }],
        }
    }
    pub fn text_only(text: impl Into<String>) -> CompletionResponse {
        CompletionResponse {
            text: text.into(),
            model: None,
            stop_reason: Some("end_turn".into()),
            tool_calls: Vec::new(),
        }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // Script mode takes precedence over reply mode so multi-turn tests
        // can pin both. Each call pops the front of the queue.
        if let Some(queue) = self.script.lock().unwrap().as_mut() {
            if queue.is_empty() {
                return Err(ProviderError::Upstream(
                    "MockProvider script exhausted".into(),
                ));
            }
            return Ok(queue.remove(0));
        }
        let text = self.reply.clone().unwrap_or_else(|| req.flatten());
        Ok(CompletionResponse {
            text,
            model: req.model,
            stop_reason: Some("end_turn".into()),
            tool_calls: Vec::new(),
        })
    }

    /// Streams the canned response: the text split into two deltas (so tests
    /// observe real incremental order), a `ToolCall` per scripted call, and a
    /// `TurnEnd` — then returns the same complete response `complete` would.
    async fn complete_streaming(
        &self,
        req: CompletionRequest,
        sink: StreamSink,
    ) -> Result<CompletionResponse, ProviderError> {
        let resp = self.complete(req).await?;
        if !resp.text.is_empty() {
            let mid = resp.text.len() / 2;
            // Split on a char boundary; fall back to one delta if the midpoint
            // lands inside a multi-byte character.
            if resp.text.is_char_boundary(mid) && mid > 0 {
                let _ = sink.send(StreamEvent::TextDelta {
                    text: resp.text[..mid].to_string(),
                });
                let _ = sink.send(StreamEvent::TextDelta {
                    text: resp.text[mid..].to_string(),
                });
            } else {
                let _ = sink.send(StreamEvent::TextDelta {
                    text: resp.text.clone(),
                });
            }
        }
        for tc in &resp.tool_calls {
            let _ = sink.send(StreamEvent::ToolCall {
                name: tc.name.clone(),
            });
        }
        let _ = sink.send(StreamEvent::TurnEnd);
        Ok(resp)
    }
}

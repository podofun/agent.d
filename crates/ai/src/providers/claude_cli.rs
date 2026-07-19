//! ClaudeCliProvider — shells out to the `claude` CLI in print mode.
//!
//! Two modes, picked automatically by inspecting the
//! [`CompletionRequest::mcp_endpoint`] the executor sets:
//!
//! 1. **Text-only fallback** (no `mcp_endpoint`): runs
//!    `claude -p [--model M] [--append-system-prompt S]` w/ the prompt on
//!    stdin and reads plain text from stdout. No tool use. This is the
//!    historical behavior and what tests that don't have a `claude` binary
//!    available exercise.
//!
//! 2. **Tool-use loop via MCP loopback** (`mcp_endpoint == Some(Http{url})`):
//!    writes a temporary MCP-config JSON pointing at the executor's
//!    loopback server, then runs
//!
//!    ```text
//!    claude -p \
//!      --output-format stream-json \
//!      --include-partial-messages \
//!      --verbose \
//!      --mcp-config <tmpfile> \
//!      --allowedTools "mcp__agentd__*" \
//!      [--model M] [--append-system-prompt S]
//!    ```
//!
//!    The CLI runs its own agent loop, calling agentd's MCP server for
//!    each tool, and emits a stream of JSONL events. We scan the stream
//!    for the final `result` event and return its `result` text.
//!
//! `LoopMode::ProviderOwned` always — the executor never sees individual
//! tool_calls from this provider because the CLI swallows them.

use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

use crate::types::{
    CompletionRequest, CompletionResponse, LoopMode, McpEndpoint, Provider, ProviderError,
};

pub struct ClaudeCliProvider {
    bin: String,
    name: String,
    extra_args: Vec<String>,
}

impl ClaudeCliProvider {
    pub fn new() -> Self {
        Self {
            bin: "claude".into(),
            name: "anthropic-cli".into(),
            extra_args: Vec::new(),
        }
    }
    pub fn with_bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }
    pub fn with_extra_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_args = args.into_iter().map(Into::into).collect();
        self
    }
}

impl Default for ClaudeCliProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for ClaudeCliProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn loop_mode(&self) -> LoopMode {
        // The CLI runs its own agent loop. When a `mcp_endpoint` is wired
        // up the CLI calls back into agentd's MCP server for tools; when
        // it isn't, the CLI is just a text-only fallback. Either way the
        // executor only sees the final assistant message — `tool_calls`
        // on the returned response stays empty.
        LoopMode::ProviderOwned
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        match &req.mcp_endpoint {
            Some(McpEndpoint::Http { url, token }) => {
                self.complete_with_mcp(req.clone(), url.clone(), token.clone())
                    .await
            }
            Some(McpEndpoint::Stdio { .. }) => Err(ProviderError::Config(
                "the claude CLI provider only supports http MCP endpoints, not stdio".into(),
            )),
            None => self.complete_text_only(req).await,
        }
    }
}

impl ClaudeCliProvider {
    /// Plain `claude -p` invocation, no tool use.
    async fn complete_text_only(
        &self,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut cmd = agentd_process::command(&self.bin);
        cmd.arg("-p");
        if let Some(model) = &req.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(system) = &req.system {
            cmd.arg("--append-system-prompt").arg(system);
        }
        for a in &self.extra_args {
            cmd.arg(a);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Transport(format!("spawn `{}`: {e}", self.bin)))?;

        let body = flatten_messages(&req);
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(body.as_bytes())
                .await
                .map_err(|e| ProviderError::Transport(format!("write stdin: {e}")))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| ProviderError::Transport(format!("close stdin: {e}")))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| ProviderError::Transport(format!("wait: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(ProviderError::Upstream(format!(
                "claude exited {}: {}",
                output.status, stderr
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string();
        if text.is_empty() {
            return Err(ProviderError::EmptyResponse);
        }
        Ok(CompletionResponse {
            text,
            model: req.model,
            stop_reason: Some("end_turn".into()),
            tool_calls: Vec::new(),
        })
    }

    /// Full agent loop. Writes an MCP config pointing at the loopback URL,
    /// passes it to `claude -p --output-format stream-json`, and extracts
    /// the final `result` text from the JSONL event stream.
    async fn complete_with_mcp(
        &self,
        req: CompletionRequest,
        mcp_url: String,
        mcp_token: String,
    ) -> Result<CompletionResponse, ProviderError> {
        // Build the MCP config file. claude CLI expects:
        //   { "mcpServers": { "<name>": { "type": "http", "url", "headers" } } }
        // The bearer header is what the loopback authenticates each call on.
        let cfg = serde_json::json!({
            "mcpServers": {
                "agentd": {
                    "type": "http",
                    "url": mcp_url,
                    "headers": { "Authorization": format!("Bearer {mcp_token}") }
                }
            }
        });
        // A `TempPath` is unique by construction and deletes itself on drop, so
        // every exit path below — spawn failure, error return, normal finish —
        // cleans up without an explicit `remove_file`. Keep `cfg_path` alive
        // until after the child exits; claude reads the file by path.
        let mut tmp = tempfile::Builder::new()
            .prefix("agentd-mcp-")
            .suffix(".json")
            .tempfile()
            .map_err(|e| {
                ProviderError::Config(format!(
                    "could not create the MCP config file for the claude CLI ({e})"
                ))
            })?;
        std::io::Write::write_all(&mut tmp, cfg.to_string().as_bytes()).map_err(|e| {
            ProviderError::Config(format!(
                "could not write the MCP config file for the claude CLI ({e})"
            ))
        })?;
        let cfg_path = tmp.into_temp_path();

        let mut cmd = agentd_process::command(&self.bin);
        cmd.arg("-p")
            .arg("--output-format")
            .arg("stream-json")
            .arg("--include-partial-messages")
            .arg("--verbose")
            .arg("--mcp-config")
            .arg(&cfg_path)
            .arg("--allowedTools")
            .arg("mcp__agentd__*");
        if let Some(model) = &req.model {
            cmd.arg("--model").arg(model);
        }
        if let Some(system) = &req.system {
            cmd.arg("--append-system-prompt").arg(system);
        }
        for a in &self.extra_args {
            cmd.arg(a);
        }
        // The CLI's MCP client aborts a tool call after ~60s by default, which
        // spuriously "times out" a tool that is still busy (a large
        // `npm install`, a build, a long shell step) even though it is making
        // progress — and leaves the agentd-side action running. Give tool calls
        // generous time; these only set a default, so an env value already in
        // scope (operator override) wins.
        for (k, default) in [("MCP_TOOL_TIMEOUT", "600000"), ("MCP_TIMEOUT", "600000")] {
            if std::env::var_os(k).is_none() {
                cmd.env(k, default);
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| ProviderError::Transport(format!("spawn `{}`: {e}", self.bin)))?;

        let body = flatten_messages(&req);
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(body.as_bytes())
                .await
                .map_err(|e| ProviderError::Transport(format!("write stdin: {e}")))?;
            stdin
                .shutdown()
                .await
                .map_err(|e| ProviderError::Transport(format!("close stdin: {e}")))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| ProviderError::Transport(format!("wait: {e}")))?;
        // `cfg_path` (the TempPath) is dropped at function end, deleting the file.

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(ProviderError::Upstream(format!(
                "claude exited {}: {}",
                output.status, stderr
            )));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let text = extract_final_text(&stdout).ok_or(ProviderError::EmptyResponse)?;
        Ok(CompletionResponse {
            text,
            model: req.model,
            stop_reason: Some("end_turn".into()),
            tool_calls: Vec::new(),
        })
    }
}

fn flatten_messages(req: &CompletionRequest) -> String {
    let mut body = String::new();
    for m in &req.messages {
        let tag = match m.role {
            crate::types::Role::System => "system",
            crate::types::Role::User => "user",
            crate::types::Role::Assistant => "assistant",
            crate::types::Role::Tool => "tool",
        };
        body.push_str(&format!("[{tag}] {}\n", m.content));
    }
    if let Some(p) = &req.prompt {
        body.push_str(p);
    }
    body
}

/// Scan claude CLI's `stream-json` output for the final assistant text.
/// claude emits a `result` event at end-of-conversation w/ `result: "..."`
/// (string) or `result: { content: [{text}] }` (object). We accept both.
/// As a last resort, we concatenate any text blocks from `assistant`
/// messages we saw along the way.
pub(crate) fn extract_final_text(stdout: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: Option<String>,
        #[serde(default)]
        result: Option<serde_json::Value>,
        #[serde(default)]
        message: Option<serde_json::Value>,
    }

    let mut last_result: Option<String> = None;
    let mut assistant_text_concat: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Event>(line) else {
            continue;
        };
        match ev.kind.as_deref() {
            Some("result") => {
                if let Some(r) = ev.result {
                    last_result = Some(stringify_result(r));
                }
            }
            Some("assistant") => {
                if let Some(msg) = ev.message
                    && let Some(text) = pluck_assistant_text(&msg)
                {
                    assistant_text_concat.push(text);
                }
            }
            _ => {}
        }
    }

    last_result.or_else(|| {
        if assistant_text_concat.is_empty() {
            None
        } else {
            Some(assistant_text_concat.join("\n"))
        }
    })
}

fn stringify_result(v: serde_json::Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_string();
    }
    if let Some(obj) = v.as_object()
        && let Some(content) = obj.get("content").and_then(|c| c.as_array())
    {
        let mut out = String::new();
        for block in content {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    v.to_string()
}

fn pluck_assistant_text(message: &serde_json::Value) -> Option<String> {
    let content = message.get("content")?.as_array()?;
    let mut out = String::new();
    for block in content {
        if block.get("type")?.as_str()? == "text"
            && let Some(t) = block.get("text").and_then(|t| t.as_str())
        {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_final_text_picks_result_string() {
        let stdout = r#"
            {"type":"system","subtype":"init"}
            {"type":"assistant","message":{"content":[{"type":"text","text":"thinking"}]}}
            {"type":"result","result":"final answer"}
        "#;
        assert_eq!(extract_final_text(stdout).as_deref(), Some("final answer"));
    }

    #[test]
    fn extract_final_text_handles_object_result() {
        let stdout = r#"
            {"type":"result","result":{"content":[{"type":"text","text":"hello"},{"type":"text","text":"world"}]}}
        "#;
        assert_eq!(extract_final_text(stdout).as_deref(), Some("hello\nworld"),);
    }

    #[test]
    fn extract_final_text_falls_back_to_assistant_blocks() {
        // No `result` event — concatenate assistant text blocks.
        let stdout = r#"
            {"type":"assistant","message":{"content":[{"type":"text","text":"part 1"}]}}
            {"type":"assistant","message":{"content":[{"type":"text","text":"part 2"}]}}
        "#;
        assert_eq!(
            extract_final_text(stdout).as_deref(),
            Some("part 1\npart 2"),
        );
    }

    #[test]
    fn extract_final_text_none_on_empty_or_garbage() {
        assert!(extract_final_text("").is_none());
        assert!(extract_final_text("not json\nstill not json").is_none());
    }

    #[test]
    fn loop_mode_is_provider_owned() {
        let p = ClaudeCliProvider::new();
        assert_eq!(p.loop_mode(), LoopMode::ProviderOwned);
    }
}

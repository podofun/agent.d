//! CodexCliProvider — thin text-only wrapper around `codex exec --json`.
//!
//! **No tool use.** Tool-use requires `CodexAppServerProvider` (which
//! drives `codex app-server` w/ a proper approval bridge into the agentd
//! permission engine). `codex exec` does not expose an allowlist
//! equivalent to claude's `--allowedTools`, so the only way to make MCP
//! calls fire non-interactively is `--dangerously-bypass-approvals-and-
//! sandbox`, which also lifts the read-only sandbox and lets codex's own
//! shell/file/web run unconstrained alongside MCP. That violates the
//! invariant that all destructive / write / network work must flow
//! through agentd's permission engine. Don't reintroduce it.
//!
//! Surface here: text-only completion. Runs
//! `codex exec --json --skip-git-repo-check -s read-only \
//!   -c approval_policy="never" [-m MODEL]` w/ the prompt on stdin.
//! `LoopMode::ProviderOwned`.

use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

use crate::types::{
    CompletionRequest, CompletionResponse, LoopMode, McpEndpoint, Provider, ProviderError,
};

pub struct CodexCliProvider {
    bin: String,
    name: String,
    extra_args: Vec<String>,
}

impl CodexCliProvider {
    pub fn new() -> Self {
        Self {
            bin: "codex".into(),
            name: "openai-cli".into(),
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

impl Default for CodexCliProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CodexCliProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn loop_mode(&self) -> LoopMode {
        LoopMode::ProviderOwned
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        if req.mcp_endpoint.is_some() {
            return Err(ProviderError::Config(
                "CodexCliProvider is text-only; tool use requires \
                 CodexAppServerProvider. `codex exec` has no allowlist \
                 equivalent to claude's --allowedTools, so MCP tool calls \
                 can only fire via --dangerously-bypass-approvals-and-\
                 sandbox, which removes the codex sandbox and breaks the \
                 agentd permission invariant."
                    .into(),
            ));
        }
        self.complete_text_only(req).await
    }
}

impl CodexCliProvider {
    async fn complete_text_only(
        &self,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut cmd = agentd_process::command(&self.bin);
        cmd.arg("exec")
            .arg("--json")
            .arg("--skip-git-repo-check")
            // Read-only sandbox + no-prompt approval keep codex's own
            // shell harmless: it can read, can't write/network, can't
            // hang on a prompt for nobody.
            .arg("-s")
            .arg("read-only")
            .arg("-c")
            .arg("approval_policy=\"never\"");
        if let Some(model) = &req.model {
            cmd.arg("-m").arg(model);
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
                "codex exited {}: {}",
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
    if let Some(s) = &req.system {
        body.push_str("[system] ");
        body.push_str(s);
        body.push('\n');
    }
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

/// Scan codex `exec --json` output for the final assistant text. Codex emits
/// `{"type":"item.completed","item":{"type":"agent_message","text":"..."}}`
/// events; we concatenate them in order.
pub(crate) fn extract_final_text(stdout: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: Option<String>,
        #[serde(default)]
        item: Option<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        #[serde(rename = "type")]
        kind: Option<String>,
        #[serde(default)]
        text: Option<String>,
    }

    let mut parts: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Event>(line) else {
            continue;
        };
        if ev.kind.as_deref() != Some("item.completed") {
            continue;
        }
        let Some(item) = ev.item else { continue };
        if item.kind.as_deref() != Some("agent_message") {
            continue;
        }
        if let Some(text) = item.text
            && !text.is_empty()
        {
            parts.push(text);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// Keep `McpEndpoint` referenced (matched in the guard above) so unused-import
// lints don't flag it.
#[allow(dead_code)]
fn _mcp_endpoint_referenced(_: &McpEndpoint) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_final_text_picks_agent_message() {
        let stdout = r#"
            {"type":"thread.started","thread_id":"abc"}
            {"type":"turn.started"}
            {"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"pong"}}
            {"type":"turn.completed","usage":{}}
        "#;
        assert_eq!(extract_final_text(stdout).as_deref(), Some("pong"));
    }

    #[test]
    fn extract_final_text_none_on_empty_or_garbage() {
        assert!(extract_final_text("").is_none());
        assert!(extract_final_text("not json\nstill not json").is_none());
        let only_tools = r#"{"type":"item.completed","item":{"type":"tool_call","text":"x"}}"#;
        assert!(extract_final_text(only_tools).is_none());
    }

    #[test]
    fn flatten_includes_system_prompt() {
        // `codex exec` has no system-prompt channel, so the provider folds it
        // into the prompt body. This is the conveyance contract the live test
        // can't reliably assert (the model may ignore an inline instruction).
        let req = crate::CompletionRequest::prompt("hi").with_system("be terse");
        let body = flatten_messages(&req);
        assert!(body.contains("[system] be terse"), "got: {body}");
        assert!(body.contains("hi"), "got: {body}");
    }

    #[test]
    fn loop_mode_is_provider_owned() {
        let p = CodexCliProvider::new();
        assert_eq!(p.loop_mode(), LoopMode::ProviderOwned);
    }

    #[test]
    fn name_is_openai_cli() {
        assert_eq!(CodexCliProvider::new().name(), "openai-cli");
    }

    #[tokio::test]
    async fn mcp_request_refused() {
        let p = CodexCliProvider::new();
        let req = crate::CompletionRequest {
            mcp_endpoint: Some(crate::McpEndpoint::Http {
                url: "http://127.0.0.1:1/mcp".into(),
                token: "test-token".into(),
            }),
            ..crate::CompletionRequest::prompt("anything")
        };
        let err = p.complete(req).await.unwrap_err();
        assert!(
            matches!(err, crate::ProviderError::Config(_)),
            "expected Config, got {err:?}"
        );
    }
}

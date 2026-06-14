use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize)]
pub struct TraceEvent {
    pub ts: String,
    pub action: String,
    pub args: serde_json::Value,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Correlation id shared by one top-level request and every child
    /// dispatch it spawns. `None` for events emitted outside a request
    /// (e.g. service lifecycle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    /// Event class: `action`, `runner`, `service`, … Lets a trace reader
    /// group an execution into its action call + per-agent runner runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl TraceEvent {
    pub fn ok(
        action: &str,
        args: serde_json::Value,
        dur_ms: u128,
        result: serde_json::Value,
    ) -> Self {
        Self {
            ts: Utc::now().to_rfc3339(),
            action: action.to_string(),
            args,
            duration_ms: dur_ms,
            result: Some(result),
            error: None,
            execution: None,
            kind: None,
        }
    }
    pub fn err(action: &str, args: serde_json::Value, dur_ms: u128, err: String) -> Self {
        Self {
            ts: Utc::now().to_rfc3339(),
            action: action.to_string(),
            args,
            duration_ms: dur_ms,
            result: None,
            error: Some(err),
            execution: None,
            kind: None,
        }
    }
    /// Chainable: stamp the correlation id (no-op when `None`).
    pub fn with_execution(mut self, execution: Option<String>) -> Self {
        self.execution = execution;
        self
    }
    /// Chainable: tag the event class.
    pub fn with_kind(mut self, kind: &str) -> Self {
        self.kind = Some(kind.to_string());
        self
    }
}

#[async_trait]
pub trait TraceSink: Send + Sync {
    async fn record(&self, event: TraceEvent);
}

#[derive(Clone)]
pub struct JsonlSink {
    path: PathBuf,
    file: Arc<Mutex<tokio::fs::File>>,
}

impl JsonlSink {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[async_trait]
impl TraceSink for JsonlSink {
    async fn record(&self, event: TraceEvent) {
        let mut line = match serde_json::to_vec(&event) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("trace serialize failed: {e}");
                return;
            }
        };
        line.push(b'\n');
        let mut f = self.file.lock().await;
        if let Err(e) = f.write_all(&line).await {
            eprintln!("trace write failed: {e}");
        }
        let _ = f.flush().await;
    }
}

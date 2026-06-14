//! Argv-only shell exec primitive.
//!
//! No shell interpreter. No string-splitting. Caller passes binary + argv.
//! This makes `context.shell.exec("git", { "diff" })` unable to express
//! `&& rm -rf /`. Composition stays explicit, never accidental.

use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Executable name or absolute path. Looked up via $PATH if not absolute.
    pub bin: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Optional stdin payload.
    #[serde(default)]
    pub stdin: Option<String>,
    /// If true, surface stderr to the caller in `stderr`. Otherwise stderr is
    /// merged into stdout (handy for tools that mix the two).
    #[serde(default = "default_true")]
    pub separate_stderr: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("spawn `{bin}`: {source}")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Run a command. The caller is responsible for permission checks BEFORE
/// invoking this function. The shell crate is a primitive; gating lives in
/// the context binding layer.
pub async fn exec(req: ExecRequest) -> Result<ExecResult, ShellError> {
    let mut cmd = Command::new(&req.bin);
    cmd.args(&req.args);
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    if req.separate_stderr {
        cmd.stderr(Stdio::piped());
    } else {
        cmd.stderr(Stdio::piped()); // captured separately, merged below
    }

    let mut child = cmd.spawn().map_err(|e| ShellError::Spawn {
        bin: req.bin.clone(),
        source: e,
    })?;

    if let (Some(input), Some(mut stdin)) = (req.stdin.as_ref(), child.stdin.take()) {
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await?;
    } else {
        drop(child.stdin.take());
    }

    let output = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);

    let (stdout, stderr) = if req.separate_stderr {
        (stdout, stderr_text)
    } else {
        let mut merged = stdout;
        if !stderr_text.is_empty() {
            if !merged.is_empty() && !merged.ends_with('\n') {
                merged.push('\n');
            }
            merged.push_str(&stderr_text);
        }
        (merged, String::new())
    };

    Ok(ExecResult {
        exit_code,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echoes_stdout() {
        let res = exec(ExecRequest {
            bin: "/bin/echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            stdin: None,
            separate_stderr: true,
        })
        .await
        .unwrap();
        assert_eq!(res.exit_code, 0);
        assert_eq!(res.stdout.trim(), "hello");
        assert!(res.stderr.is_empty());
    }

    #[tokio::test]
    async fn captures_nonzero_exit() {
        let res = exec(ExecRequest {
            bin: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
            cwd: None,
            stdin: None,
            separate_stderr: true,
        })
        .await
        .unwrap();
        assert_eq!(res.exit_code, 7);
    }

    #[tokio::test]
    async fn separates_stderr() {
        let res = exec(ExecRequest {
            bin: "/bin/sh".into(),
            args: vec!["-c".into(), "echo out; echo err >&2".into()],
            cwd: None,
            stdin: None,
            separate_stderr: true,
        })
        .await
        .unwrap();
        assert_eq!(res.stdout.trim(), "out");
        assert_eq!(res.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn merges_stderr_when_requested() {
        let res = exec(ExecRequest {
            bin: "/bin/sh".into(),
            args: vec!["-c".into(), "echo out; echo err >&2".into()],
            cwd: None,
            stdin: None,
            separate_stderr: false,
        })
        .await
        .unwrap();
        assert!(res.stdout.contains("out"));
        assert!(res.stdout.contains("err"));
        assert!(res.stderr.is_empty());
    }

    #[tokio::test]
    async fn pipes_stdin() {
        let res = exec(ExecRequest {
            bin: "/bin/cat".into(),
            args: vec![],
            cwd: None,
            stdin: Some("payload".into()),
            separate_stderr: true,
        })
        .await
        .unwrap();
        assert_eq!(res.stdout.trim(), "payload");
    }

    #[tokio::test]
    async fn missing_binary_returns_spawn_error() {
        let err = exec(ExecRequest {
            bin: "/nonexistent/agentd-shell-test".into(),
            args: vec![],
            cwd: None,
            stdin: None,
            separate_stderr: true,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, ShellError::Spawn { .. }));
    }

    #[tokio::test]
    async fn respects_cwd() {
        let res = exec(ExecRequest {
            bin: "/bin/pwd".into(),
            args: vec![],
            cwd: Some(PathBuf::from("/tmp")),
            stdin: None,
            separate_stderr: true,
        })
        .await
        .unwrap();
        assert_eq!(res.stdout.trim(), "/tmp");
    }
}

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

pub mod dns_pin;
pub mod dns_wire;
pub mod gateway;
pub mod netfilter;
pub mod policy;
pub mod sandbox;
pub use policy::{SandboxError, SandboxPolicy};

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
    /// Native-OS sandbox policy applied to the child. `None` = no sandbox
    /// (internal callers only; `ctx.shell` always sets `Some`). Host-derived,
    /// never wire data, so it is skipped during (de)serialization.
    #[serde(skip, default)]
    pub sandbox: Option<SandboxPolicy>,
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
    #[error("native shell sandbox unavailable; shell denied")]
    SandboxUnavailable,
    #[error("sandbox setup failed: {0}")]
    Sandbox(String),
}

/// Run a command. The caller is responsible for permission checks BEFORE
/// invoking this function. The shell crate is a primitive; gating lives in
/// the context binding layer.
pub async fn exec(req: ExecRequest) -> Result<ExecResult, ShellError> {
    // Fail closed: if a sandbox policy is requested but no backend can enforce
    // it, refuse to run rather than spawn unconfined.
    if let Some(policy) = &req.sandbox
        && !policy.unrestricted
        && !sandbox::is_supported()
    {
        return Err(ShellError::SandboxUnavailable);
    }

    // Host-granular network is enforced ONLY by the Linux transparent backend
    // (netns + nft redirect + DNS-pin, protocol-agnostic, no proxy env). When a
    // policy permits network:
    //   - Linux: run the child in the transparent netns; this owns the whole
    //     spawn and returns.
    //   - other OSes: no transparent backend yet -> fail closed (never spawn with
    //     unconfined network).
    if let Some(policy) = req.sandbox.clone()
        && !policy.unrestricted
        && policy.allow_net
    {
        #[cfg(target_os = "linux")]
        {
            if !sandbox::net_supported() {
                return Err(ShellError::SandboxUnavailable);
            }
            let (host_grants, literal_ips) = dns_pin::split_grants(&policy.net_hosts);
            return sandbox::linux_transparent::run_contained(
                &req,
                &policy,
                host_grants,
                literal_ips,
            )
            .await;
        }
        // Windows enforces host-granular net via WFP filters scoped to the
        // child's AppContainer package SID (handled inside run_contained).
        #[cfg(target_os = "windows")]
        return sandbox::windows_run_contained(&req, &policy).await;
        // macOS enforces via pf scoped to a dedicated UID.
        #[cfg(target_os = "macos")]
        return sandbox::macos_run_contained(&req, &policy).await;
        #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
        return Err(ShellError::SandboxUnavailable);
    }

    // No network (or unrestricted): apply the filesystem sandbox. The OS profile
    // denies network by construction (Landlock/Seatbelt/AppContainer), so no
    // egress path exists.
    #[cfg(target_os = "windows")]
    if let Some(policy) = req.sandbox.clone()
        && !policy.unrestricted
    {
        return sandbox::windows_run_contained(&req, &policy).await;
    }

    // macOS enforces via an argv wrapper (sandbox-exec), chosen before spawn.
    #[cfg(target_os = "macos")]
    let (bin, args): (String, Vec<String>) = match &req.sandbox {
        Some(p) if !p.unrestricted => sandbox::wrap_argv(p, &req.bin, &req.args),
        _ => (req.bin.clone(), req.args.clone()),
    };
    #[cfg(not(target_os = "macos"))]
    let (bin, args): (String, Vec<String>) = (req.bin.clone(), req.args.clone());

    let mut cmd = Command::new(&bin);
    cmd.args(&args);

    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }

    // Linux enforces by restricting the forked child before exec.
    #[cfg(target_os = "linux")]
    if let Some(policy) = req.sandbox.clone()
        && !policy.unrestricted
    {
        // `cmd` is a `tokio::process::Command` with its OWN inherent `pre_exec`;
        // do not import `std::os::unix::process::CommandExt`.
        // SAFETY: the closure runs in the forked child before exec. It only
        // calls Landlock syscalls + path opens; no shared-state mutation.
        unsafe {
            cmd.pre_exec(move || {
                sandbox::apply(&policy)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))
            });
        }
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

// These exercise real unix binaries (/bin/echo, /bin/sh, ...); gated to unix.
#[cfg(all(test, unix))]
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
            sandbox: None,
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
            sandbox: None,
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
            sandbox: None,
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
            sandbox: None,
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
            sandbox: None,
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
            sandbox: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, ShellError::Spawn { .. }));
    }

    // macOS resolves /tmp -> /private/tmp, so pin to Linux.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn respects_cwd() {
        let res = exec(ExecRequest {
            bin: "/bin/pwd".into(),
            args: vec![],
            cwd: Some(PathBuf::from("/tmp")),
            stdin: None,
            separate_stderr: true,
            sandbox: None,
        })
        .await
        .unwrap();
        assert_eq!(res.stdout.trim(), "/tmp");
    }
}

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
mod kill_tree;
#[cfg(target_os = "windows")]
pub mod netbroker;
pub mod netfilter;
pub mod policy;
mod program;
mod reach;
pub mod sandbox;
pub use policy::{SandboxError, SandboxPolicy};
pub use program::resolve_program;
pub use reach::{Overlap, Reach};

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
    /// Stop the program, and everything it started, once this many
    /// milliseconds pass. `None` lets it run until it exits.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
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
    #[error("could not start `{bin}` ({source})")]
    Spawn {
        bin: String,
        #[source]
        source: std::io::Error,
    },
    #[error("I/O error ({0})")]
    Io(#[from] std::io::Error),
    #[cfg_attr(
        target_os = "linux",
        error(
            "the native shell sandbox is unavailable because this kernel does not enforce Landlock, so shell commands are denied. See https://docs.podo.fun/agentd/v0/security/sandbox"
        )
    )]
    #[cfg_attr(
        not(target_os = "linux"),
        error(
            "the native shell sandbox is unavailable, so shell commands are denied. Run `agentd --install-sandbox` to set it up"
        )
    )]
    SandboxUnavailable,
    #[error(
        "shell commands with a `net:` grant need unprivileged user and network namespaces, which this host or container does not allow, so the command was denied. See https://docs.podo.fun/agentd/v0/security/sandbox"
    )]
    NetSandboxUnavailable,
    #[error("the shell sandbox could not be set up ({0})")]
    Sandbox(String),
    /// The child could reach a path the operator denies, so it was not started.
    #[error("{0}")]
    DeniedReach(String),
    /// The program ran past `timeout_ms`, so it and everything it started were
    /// killed.
    #[error("`{bin}` was stopped after {}", format_duration(*.timeout_ms))]
    TimedOut { bin: String, timeout_ms: u64 },
}

/// Render a timeout the way a person would say it: `250 ms`, `120 s`, `1.5 s`.
fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// Run a command. The caller is responsible for permission checks BEFORE
/// invoking this function. The shell crate is a primitive; gating lives in
/// the context binding layer.
///
/// Dropping the returned future kills the program and everything it started,
/// on every backend. A `timeout_ms` is that same drop on a deadline.
pub async fn exec(req: ExecRequest) -> Result<ExecResult, ShellError> {
    let Some(timeout_ms) = req.timeout_ms else {
        return run(req).await;
    };
    let bin = req.bin.clone();
    tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), run(req))
        .await
        .unwrap_or(Err(ShellError::TimedOut { bin, timeout_ms }))
}

async fn run(req: ExecRequest) -> Result<ExecResult, ShellError> {
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
                return Err(ShellError::NetSandboxUnavailable);
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
        // macOS enforces via the transparent pf-broker path: same DNS-pin +
        // PermitSet core as Linux, capture via pf redirect to a daemon relay.
        #[cfg(target_os = "macos")]
        {
            if !sandbox::net_supported() {
                return Err(ShellError::SandboxUnavailable);
            }
            let (host_grants, literal_ips) = dns_pin::split_grants(&policy.net_hosts);
            return sandbox::macos_run_contained(&req, &policy, host_grants, literal_ips).await;
        }
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

    // The child leads a new process group that everything it starts joins, so
    // the whole tree can be killed at once.
    #[cfg(unix)]
    cmd.process_group(0);
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| ShellError::Spawn {
        bin: req.bin.clone(),
        source: e,
    })?;
    let tree = kill_tree::KillSwitch::default();
    // Declared after `child`, so on drop it fires while the leader is unreaped.
    let _kill_on_drop = tree.on_drop();
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        tree.arm(pid as i32);
    }
    // Windows has no process groups; a job assigned right after spawn catches
    // everything the child starts from here on.
    #[cfg(windows)]
    if let Some(handle) = child.raw_handle()
        && let Ok(job) = kill_tree::Job::new()
        && job
            .assign(windows::Win32::Foundation::HANDLE(handle))
            .is_ok()
    {
        tree.arm(std::sync::Arc::new(job));
    }

    if let (Some(input), Some(mut stdin)) = (req.stdin.as_ref(), child.stdin.take()) {
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await?;
    } else {
        drop(child.stdin.take());
    }

    let output = child.wait_with_output().await?;
    tree.disarm();
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
            timeout_ms: None,
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
            timeout_ms: None,
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
            timeout_ms: None,
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
            timeout_ms: None,
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
            timeout_ms: None,
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
            timeout_ms: None,
            sandbox: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, ShellError::Spawn { .. }));
    }

    /// A script that records the pid of a background `sleep` it starts, then
    /// blocks in a foreground `sleep`, so both the direct child and a
    /// grandchild are alive until something kills them.
    fn tree_req(pid_file: &std::path::Path, timeout_ms: Option<u64>) -> ExecRequest {
        ExecRequest {
            bin: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                format!("sleep 30 & echo $! > {}; sleep 30", pid_file.display()),
            ],
            cwd: None,
            stdin: None,
            separate_stderr: true,
            timeout_ms,
            sandbox: None,
        }
    }

    async fn read_pid(pid_file: &std::path::Path) -> i32 {
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(pid_file)
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("the script never wrote its pid file");
    }

    /// Whether `pid` still runs, as `kill -0` sees it. A zombie waiting for its
    /// parent to reap it no longer runs, so it counts as gone.
    fn running(pid: i32) -> bool {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|s| {
                s.rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z')
            })
            .unwrap_or(false);
        alive && !zombie
    }

    async fn assert_stops(pid: i32) {
        for _ in 0..200 {
            if !running(pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("process {pid} is still running");
    }

    #[tokio::test]
    async fn timeout_kills_the_program_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let started = std::time::Instant::now();
        let err = exec(tree_req(&pid_file, Some(300))).await.unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(matches!(err, ShellError::TimedOut { .. }), "got {err}");
        assert_eq!(err.to_string(), "`/bin/sh` was stopped after 300 ms");
        assert_stops(read_pid(&pid_file).await).await;
    }

    #[tokio::test]
    async fn dropping_the_exec_future_kills_the_program_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let task = tokio::spawn(exec(tree_req(&pid_file, None)));
        let grandchild = read_pid(&pid_file).await;
        assert!(running(grandchild));
        task.abort();
        let _ = task.await;
        assert_stops(grandchild).await;
    }

    #[tokio::test]
    async fn a_program_that_finishes_in_time_is_not_affected() {
        let res = exec(ExecRequest {
            bin: "/bin/echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            stdin: None,
            separate_stderr: true,
            timeout_ms: Some(10_000),
            sandbox: None,
        })
        .await
        .unwrap();
        assert_eq!(res.stdout.trim(), "hello");
    }

    #[test]
    fn timeouts_read_like_a_person_would_say_them() {
        assert_eq!(format_duration(250), "250 ms");
        assert_eq!(format_duration(120_000), "120 s");
        assert_eq!(format_duration(1500), "1.5 s");
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
            timeout_ms: None,
            sandbox: None,
        })
        .await
        .unwrap();
        assert_eq!(res.stdout.trim(), "/tmp");
    }
}

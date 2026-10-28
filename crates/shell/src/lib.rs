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
mod output;
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
    /// The program's exit code. Meaningless when `timed_out` is set, since the
    /// program was killed rather than allowed to exit.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// The program ran past `timeout_ms`, so it and everything it started were
    /// killed. `stdout` and `stderr` hold what it wrote before that.
    #[serde(default)]
    pub timed_out: bool,
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
    /// `timeout_ms` was zero, which would stop the program before it started.
    #[error("a shell timeout must be at least 1 ms")]
    InvalidTimeout,
    /// The exec was cancelled or timed out before the program started, so it
    /// never ran. `exec` reports a timeout as a timed-out result instead.
    #[error("the command was stopped before it started")]
    Stopped,
}

/// How long a timed-out exec waits, after killing the tree, for the backend to
/// hand back the output it read. Only a process that left the tree (for example
/// with `setsid`) and still holds the output pipes makes it wait this long.
const OUTPUT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Run a command. The caller is responsible for permission checks BEFORE
/// invoking this function. The shell crate is a primitive; gating lives in
/// the context binding layer.
///
/// Dropping the returned future kills the program and everything it started,
/// on every backend. When `timeout_ms` passes, the same kill happens and the
/// result comes back with `timed_out` set and the output read so far.
pub async fn exec(req: ExecRequest) -> Result<ExecResult, ShellError> {
    if req.timeout_ms == Some(0) {
        return Err(ShellError::InvalidTimeout);
    }
    let tree = kill_tree::KillSwitch::default();
    let deadline = req.timeout_ms.map(std::time::Duration::from_millis);
    // A confined command must not leave anything behind holding its grants,
    // so once it is done the rest of its tree goes too. Unconfined commands
    // may daemonize on purpose and keep that freedom.
    let confined = req.sandbox.as_ref().is_some_and(|p| !p.unrestricted);
    let run = run(req, tree.clone());
    tokio::pin!(run);
    // Declared after `run`, so on drop it fires first, while the backend still
    // owns the unreaped group leader.
    let guard = tree.on_drop();

    let Some(deadline) = deadline else {
        let res = run.await;
        if res.is_err() || confined {
            tree.fire();
        }
        guard.defuse();
        return res;
    };
    let res = match tokio::time::timeout(deadline, &mut run).await {
        Ok(res) => {
            if res.is_err() || confined {
                tree.fire();
            }
            res
        }
        Err(_) => {
            tree.fire();
            let collected = tokio::time::timeout(OUTPUT_GRACE, &mut run).await;
            let (stdout, stderr) = match collected {
                Ok(Ok(res)) => (res.stdout, res.stderr),
                _ => (String::new(), String::new()),
            };
            Ok(ExecResult {
                exit_code: -1,
                stdout,
                stderr,
                timed_out: true,
            })
        }
    };
    guard.defuse();
    res
}

async fn run(req: ExecRequest, tree: kill_tree::KillSwitch) -> Result<ExecResult, ShellError> {
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
                tree,
            )
            .await;
        }
        // Windows enforces host-granular net via WFP filters scoped to the
        // child's AppContainer package SID (handled inside run_contained).
        #[cfg(target_os = "windows")]
        return sandbox::windows_run_contained(&req, &policy, tree).await;
        // macOS enforces via the transparent pf-broker path: same DNS-pin +
        // PermitSet core as Linux, capture via pf redirect to a daemon relay.
        #[cfg(target_os = "macos")]
        {
            if !sandbox::net_supported() {
                return Err(ShellError::SandboxUnavailable);
            }
            let (host_grants, literal_ips) = dns_pin::split_grants(&policy.net_hosts);
            return sandbox::macos_run_contained(&req, &policy, host_grants, literal_ips, tree)
                .await;
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
        return sandbox::windows_run_contained(&req, &policy, tree).await;
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
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        tree.arm(kill_tree::kill_group(pid as i32))?;
    }
    // Windows has no process groups. This path is unsandboxed, so a job
    // assigned right after spawn is best effort: it catches everything the
    // child starts from here on, and `kill_on_drop` still stops the child.
    #[cfg(windows)]
    if let Some(handle) = child.raw_handle()
        && let Ok(job) = kill_tree::Job::new()
        && job
            .assign(windows::Win32::Foundation::HANDLE(handle))
            .is_ok()
    {
        tree.arm(job.into_kill())?;
    }

    // Feed stdin while draining both outputs. Doing them one after another
    // deadlocks once the input and the output both exceed the pipe buffer:
    // the child blocks writing, we block writing, nobody reads.
    let stdin_pipe = child.stdin.take();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let feed = async {
        if let (Some(input), Some(mut stdin)) = (req.stdin.as_ref(), stdin_pipe) {
            match stdin.write_all(input.as_bytes()).await {
                Ok(()) => stdin.shutdown().await?,
                // The program exited, or closed stdin, before reading it all.
                // That is its business; its exit code tells the story.
                Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(e) => return Err(e),
            }
        }
        Ok::<(), std::io::Error>(())
    };
    async fn drain<R: tokio::io::AsyncRead + Unpin>(pipe: Option<R>) -> std::io::Result<output::Capped> {
        match pipe {
            Some(p) => output::Capped::read_async(p).await,
            None => Ok(output::Capped::default()),
        }
    }
    let (fed, out, err) = tokio::join!(feed, drain(stdout_pipe), drain(stderr_pipe));
    fed?;
    let stdout = out?.into_text();
    let stderr_text = err?.into_text();
    let status = child.wait().await?;
    let exit_code = status.code().unwrap_or(-1);

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
        timed_out: false,
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
                format!(
                    "echo started; sleep 30 & echo $! > {}; sleep 30",
                    pid_file.display()
                ),
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
        let task = tokio::spawn(exec(tree_req(&pid_file, Some(1500))));
        let grandchild = read_pid(&pid_file).await;
        let res = task.await.unwrap().unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(res.timed_out, "{res:?}");
        assert_eq!(
            res.stdout.trim(),
            "started",
            "output before the kill is kept"
        );
        assert_stops(grandchild).await;
    }

    #[tokio::test]
    async fn a_timeout_that_fires_at_once_still_stops_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let res = exec(ExecRequest {
            bin: "/bin/sh".into(),
            args: vec!["-c".into(), format!("sleep 2; touch {}", marker.display())],
            cwd: None,
            stdin: None,
            separate_stderr: true,
            timeout_ms: Some(1),
            sandbox: None,
        })
        .await
        .unwrap();
        assert!(res.timed_out, "{res:?}");
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "the program kept running after its timeout"
        );
    }

    #[tokio::test]
    async fn a_zero_timeout_is_refused() {
        let err = exec(ExecRequest {
            bin: "/bin/echo".into(),
            args: vec![],
            cwd: None,
            stdin: None,
            separate_stderr: true,
            timeout_ms: Some(0),
            sandbox: None,
        })
        .await
        .unwrap_err();
        assert!(matches!(err, ShellError::InvalidTimeout), "got {err}");
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

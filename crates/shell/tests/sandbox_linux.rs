#![cfg(target_os = "linux")]
//! Real Landlock enforcement tests. Skipped automatically when the kernel lacks
//! Landlock (is_supported() == false) so CI on old kernels stays green.

use agentd_shell::sandbox::is_supported;
use agentd_shell::{ExecRequest, SandboxPolicy, exec};

fn policy_writing_only(dir: &std::path::Path) -> SandboxPolicy {
    SandboxPolicy {
        read_paths: vec![], // baseline added inside the backend
        write_paths: vec![dir.to_path_buf()],
        allow_net: false,
        net_hosts: vec![],
        unrestricted: false,
    }
}

fn req(bin: &str, args: Vec<String>, policy: SandboxPolicy) -> ExecRequest {
    ExecRequest {
        bin: bin.into(),
        args,
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(policy),
    }
}

#[tokio::test]
async fn write_inside_grant_succeeds() {
    if !is_supported() {
        eprintln!("landlock unsupported; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("ok.txt");
    let res = exec(req(
        "/bin/sh",
        vec!["-c".into(), format!("echo hi > {}", target.display())],
        policy_writing_only(dir.path()),
    ))
    .await
    .unwrap();
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(target.exists());
}

#[tokio::test]
async fn write_outside_grant_is_denied() {
    if !is_supported() {
        eprintln!("landlock unsupported; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap(); // granted
    let outside = tempfile::tempdir().unwrap(); // NOT granted
    let target = outside.path().join("nope.txt");
    let res = exec(req(
        "/bin/sh",
        vec!["-c".into(), format!("echo hi > {}", target.display())],
        policy_writing_only(dir.path()),
    ))
    .await
    .unwrap();
    assert_ne!(res.exit_code, 0, "write outside grant must fail");
    assert!(!target.exists());
}

#[tokio::test]
async fn binary_still_runs_under_read_baseline() {
    if !is_supported() {
        eprintln!("landlock unsupported; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let res = exec(req(
        "/bin/echo",
        vec!["alive".into()],
        policy_writing_only(dir.path()),
    ))
    .await
    .unwrap();
    assert_eq!(res.stdout.trim(), "alive");
}

/// Ordinary binaries read `/proc/self/*` (status, maps, exe, fd) and system
/// files like `/proc/cpuinfo`. That must work for any process in the sandboxed
/// tree, not only the one exec'd directly: here `cat` is a child of `sh`.
#[tokio::test]
async fn descendants_can_read_their_own_proc_entries() {
    if !is_supported() {
        eprintln!("landlock unsupported; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let res = exec(req(
        "/bin/sh",
        vec![
            "-c".into(),
            "cat /proc/self/status >/dev/null && readlink /proc/self/exe >/dev/null \
             && ls /proc/self/fd >/dev/null && head -c1 /proc/cpuinfo >/dev/null && echo OK"
                .into(),
        ],
        policy_writing_only(dir.path()),
    ))
    .await
    .unwrap();
    assert_eq!(res.stdout.trim(), "OK", "stderr: {}", res.stderr);
}

/// Reading `/proc` must not expose processes outside the sandbox: Landlock
/// refuses ptrace-gated entries (environ, maps, mem, fd targets, cwd, root,
/// exe) of any process that is not inside the child's own sandbox domain,
/// including its parent.
#[tokio::test]
async fn other_processes_proc_secrets_stay_denied() {
    if !is_supported() {
        eprintln!("landlock unsupported; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let me = std::process::id();
    let res = exec(req(
        "/bin/sh",
        vec![
            "-c".into(),
            format!(
                "for f in /proc/{me}/environ /proc/{me}/maps /proc/{me}/mem /proc/1/environ; do \
                 if head -c1 $f >/dev/null 2>&1; then echo LEAK $f; fi; done; \
                 for l in /proc/{me}/fd/0 /proc/{me}/cwd /proc/{me}/root /proc/{me}/exe; do \
                 if readlink $l >/dev/null 2>&1; then echo LEAK $l; fi; done; \
                 if ls /proc/{me}/cwd/ >/dev/null 2>&1; then echo LEAK cwd-dir; fi; echo DONE"
            ),
        ],
        policy_writing_only(dir.path()),
    ))
    .await
    .unwrap();
    assert!(!res.stdout.contains("LEAK"), "{res:?}");
    assert!(res.stdout.contains("DONE"), "{res:?}");
}

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


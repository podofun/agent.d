#![cfg(target_os = "macos")]
//! Adversarial sandbox-escape tests for the macOS Seatbelt backend. Each test
//! makes the sandboxed child *attempt* a known escape and asserts it is blocked.
//!
//! Covered vectors: `..` traversal, symlink-out-of-grant, reading an ungranted
//! file, outbound network when denied, and a child-spawned grandchild (the
//! Seatbelt profile confines the whole process tree).

use agentd_shell::sandbox::is_supported;
use agentd_shell::{ExecRequest, SandboxPolicy, exec};

fn write_policy(dir: &std::path::Path) -> SandboxPolicy {
    SandboxPolicy {
        read_paths: vec![],
        write_paths: vec![dir.to_path_buf()],
        allow_net: false,
        net_hosts: vec![],
        unrestricted: false,
    }
}

fn sh(script: String, policy: SandboxPolicy) -> ExecRequest {
    ExecRequest {
        bin: "/bin/sh".into(),
        args: vec!["-c".into(), script],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(policy),
    }
}

fn skip() -> bool {
    if !is_supported() {
        eprintln!("sandbox-exec unavailable; skipping");
        return true;
    }
    false
}

#[tokio::test]
async fn write_via_parent_traversal_is_denied() {
    if skip() {
        return;
    }
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("pwned");
    let escape = format!(
        "{}/../{}/pwned",
        granted.path().display(),
        outside.path().file_name().unwrap().to_string_lossy()
    );
    let res = exec(sh(
        format!("echo pwned > '{escape}'"),
        write_policy(granted.path()),
    ))
    .await
    .unwrap();
    assert_ne!(res.exit_code, 0, "traversal write must fail");
    assert!(!target.exists(), "file outside grant must not exist");
}

#[tokio::test]
async fn write_through_symlink_out_of_grant_is_denied() {
    if skip() {
        return;
    }
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let link = granted.path().join("escape");
    std::os::unix::fs::symlink(outside.path(), &link).unwrap();
    let target = outside.path().join("pwned");
    let res = exec(sh(
        format!("echo pwned > '{}/pwned'", link.display()),
        write_policy(granted.path()),
    ))
    .await
    .unwrap();
    assert_ne!(res.exit_code, 0, "symlinked write must fail");
    assert!(!target.exists(), "file outside grant must not exist");
}

#[tokio::test]
async fn read_outside_grant_is_denied() {
    if skip() {
        return;
    }
    let granted = tempfile::tempdir().unwrap();
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().join("secret.txt");
    std::fs::write(&secret, "TOPSECRET").unwrap();
    let res = exec(sh(
        format!("cat '{}'", secret.display()),
        write_policy(granted.path()),
    ))
    .await
    .unwrap();
    assert!(
        !res.stdout.contains("TOPSECRET"),
        "secret leaked: {:?}",
        res.stdout
    );
    assert_ne!(res.exit_code, 0, "read outside grant must fail");
}

#[tokio::test]
async fn net_denied_blocks_outbound() {
    if skip() {
        return;
    }
    // One-shot responder: reachable -> curl exits 0, blocked -> non-zero.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            use std::io::Write;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let res = exec(ExecRequest {
        bin: "/usr/bin/curl".into(),
        args: vec![
            "-s".into(),
            "-m".into(),
            "5".into(),
            "-o".into(),
            "/dev/null".into(),
            format!("http://127.0.0.1:{port}/"),
        ],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(write_policy(dir.path())),
    })
    .await
    .unwrap();
    assert_ne!(
        res.exit_code, 0,
        "net-denied child reached the loopback responder; stderr: {}",
        res.stderr
    );
}

#[tokio::test]
async fn grandchild_inherits_filesystem_confinement() {
    if skip() {
        return;
    }
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("pwned");
    let res = exec(sh(
        format!("/bin/sh -c \"echo pwned > '{}'\"", target.display()),
        write_policy(granted.path()),
    ))
    .await
    .unwrap();
    assert!(!target.exists(), "grandchild escaped fs confinement");
    let _ = res;
}

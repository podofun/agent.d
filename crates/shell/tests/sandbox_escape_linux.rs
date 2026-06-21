#![cfg(target_os = "linux")]
//! Adversarial sandbox-escape tests for the Linux backend. Each test makes the
//! sandboxed child *attempt* a known escape and asserts it is blocked. Skipped
//! when the kernel lacks Landlock (is_supported() == false) so old-kernel CI
//! stays green.
//!
//! Covered vectors:
//! - filesystem: `..` traversal, symlink-out-of-grant, reading an ungranted file;
//! - network (deny): TCP to a host listener, UDP to a host listener, and a
//!   child-spawned grandchild — confinement is inherited across exec/fork.

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

fn py(code: String, policy: SandboxPolicy) -> ExecRequest {
    ExecRequest {
        bin: "/usr/bin/python3".into(),
        args: vec!["-c".into(), code],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(policy),
    }
}

fn skip() -> bool {
    if !is_supported() {
        eprintln!("landlock unsupported; skipping");
        return true;
    }
    false
}

// ---------- filesystem ----------

/// `..` cannot climb out of a granted write subtree.
#[tokio::test]
async fn write_via_parent_traversal_is_denied() {
    if skip() {
        return;
    }
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let escape = format!("{}/../{}/pwned", granted.path().display(), {
        outside.path().file_name().unwrap().to_string_lossy()
    });
    // Note: the parent of both tempdirs is the same /tmp; `..` lands in a sibling
    // that is NOT granted, so Landlock must deny the write.
    let target = outside.path().join("pwned");
    let res = exec(sh(
        format!("echo pwned > '{escape}'"),
        write_policy(granted.path()),
    ))
    .await
    .unwrap();
    assert_ne!(res.exit_code, 0, "traversal write must fail");
    assert!(!target.exists(), "file outside grant must not exist");
}

/// A symlink inside the grant pointing outside cannot be used to write out.
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

/// A file outside every read grant (and outside the read baseline) is unreadable.
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

// ---------- network (denied) ----------

/// With network denied the child cannot reach a TCP listener on the host
/// loopback — not even 127.0.0.1.
#[tokio::test]
async fn net_denied_blocks_tcp_to_host_loopback() {
    if skip() {
        return;
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let _ = listener.accept();
    });
    let dir = tempfile::tempdir().unwrap();
    let code = format!(
        "import socket,sys\n\
         try:\n  socket.create_connection(('127.0.0.1',{port}),2); print('CONNECTED')\n\
         except Exception as e:\n  print('blocked', e); sys.exit(7)\n"
    );
    let res = exec(py(code, write_policy(dir.path()))).await.unwrap();
    assert!(
        !res.stdout.contains("CONNECTED"),
        "TCP escape to host loopback; stdout={:?} stderr={:?}",
        res.stdout,
        res.stderr
    );
}

/// With network denied the child cannot deliver a UDP datagram to a socket on
/// the host loopback. Landlock alone does NOT cover UDP, so this guards the
/// seccomp filter that blocks IP-socket creation on the deny path.
#[tokio::test]
async fn net_denied_blocks_udp_to_host_loopback() {
    if skip() {
        return;
    }
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let port = sock.local_addr().unwrap().port();
    let recv = std::thread::spawn(move || {
        let mut buf = [0u8; 16];
        sock.recv_from(&mut buf)
            .map(|(n, _)| buf[..n].to_vec())
            .ok()
    });
    let dir = tempfile::tempdir().unwrap();
    let code = format!(
        "import socket\n\
         s=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)\n\
         try:\n  s.sendto(b'PWNED',('127.0.0.1',{port})); print('SENT')\n\
         except Exception as e:\n  print('blocked', e)\n"
    );
    let _ = exec(py(code, write_policy(dir.path()))).await.unwrap();
    let got = recv.join().unwrap();
    assert!(
        got.is_none(),
        "UDP datagram escaped the sandbox to the host loopback: {got:?}"
    );
}

/// A grandchild (child spawns another process) is still confined: the sandbox is
/// inherited across fork/exec.
#[tokio::test]
async fn grandchild_inherits_filesystem_confinement() {
    if skip() {
        return;
    }
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("pwned");
    // Outer sh spawns inner sh, which attempts the escaping write.
    let res = exec(sh(
        format!("/bin/sh -c \"echo pwned > '{}'\"", target.display()),
        write_policy(granted.path()),
    ))
    .await
    .unwrap();
    assert!(!target.exists(), "grandchild escaped fs confinement");
    let _ = res;
}

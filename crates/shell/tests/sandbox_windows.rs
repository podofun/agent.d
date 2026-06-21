#![cfg(target_os = "windows")]
//! Real AppContainer enforcement tests for the Windows backend.
//!
//! Coverage:
//! - a DLL-heavy binary (PowerShell) still initializes inside the AppContainer
//!   (system DLLs grant `ALL_APPLICATION_PACKAGES`, and the stdio pipes are
//!   ACL'd for the package so I/O works); `STATUS_DLL_INIT_FAILED`
//!   (`0xC0000142` => `-1073741502`) before `main` would mean we broke startup;
//! - writes land only inside the granted scratch dir, never outside;
//! - with `allow_net = false` the child has no outbound network at all.

use agentd_shell::sandbox::is_supported;
use agentd_shell::{ExecRequest, SandboxPolicy, exec};

const STATUS_DLL_INIT_FAILED: i32 = -1073741502; // 0xC0000142

fn policy(write: &std::path::Path) -> SandboxPolicy {
    SandboxPolicy {
        read_paths: vec![],
        write_paths: vec![write.to_path_buf()],
        allow_net: false,
        net_hosts: vec![],
        unrestricted: false,
    }
}

fn req(bin: String, args: Vec<String>, policy: SandboxPolicy) -> ExecRequest {
    ExecRequest {
        bin,
        args,
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(policy),
    }
}

/// Absolute path to Windows PowerShell — a DLL-heavy binary that loads the
/// `user32`/`gdi32`/CLR stack at startup, so it exercises window-station access
/// during DLL init. Always present on Windows.
fn powershell() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe")
}

#[tokio::test]
async fn dll_heavy_binary_initializes_under_sandbox() {
    assert!(is_supported(), "windows sandbox must be supported");

    let dir = tempfile::tempdir().unwrap();
    let res = exec(req(
        powershell(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            // A cmdlet, not a bare .NET call: the restricted token puts PowerShell
            // in Constrained Language Mode, which forbids arbitrary method calls.
            "Write-Output alive".into(),
        ],
        policy(dir.path()),
    ))
    .await
    .unwrap();

    assert_ne!(
        res.exit_code, STATUS_DLL_INIT_FAILED,
        "child died at DLL init under the sandbox \
         (restricted token lacks window-station/desktop access)"
    );
    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(
        res.stdout.contains("alive"),
        "expected child stdout, got: {:?}",
        res.stdout
    );
}

/// A minimal write inside the granted scratch dir must still succeed: the
/// window-station fix must not loosen the write-restriction confinement.
#[tokio::test]
async fn write_inside_grant_succeeds() {
    assert!(is_supported(), "windows sandbox must be supported");

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("ok.txt");
    let script = format!(
        "Set-Content -LiteralPath '{}' -Value 'hi' -NoNewline",
        target.display()
    );
    let res = exec(req(
        powershell(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        policy(dir.path()),
    ))
    .await
    .unwrap();

    assert_eq!(res.exit_code, 0, "stderr: {}", res.stderr);
    assert!(target.exists(), "write inside grant should have landed");
}

/// A write to a directory that was NOT granted must fail. Guards the
/// confinement boundary: a lowbox child can only touch paths whose ACL grants
/// the AppContainer package SID, which we stamp only on the granted scratch dir.
#[tokio::test]
async fn write_outside_grant_is_denied() {
    assert!(is_supported(), "windows sandbox must be supported");

    let granted = tempfile::tempdir().unwrap(); // the only writable subtree
    let outside = tempfile::tempdir().unwrap(); // NOT granted
    let target = outside.path().join("nope.txt");
    let script = format!(
        "Set-Content -LiteralPath '{}' -Value 'x' -NoNewline",
        target.display()
    );
    let res = exec(req(
        powershell(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        policy(granted.path()),
    ))
    .await
    .unwrap();

    assert_ne!(res.exit_code, 0, "write outside grant must fail");
    assert!(!target.exists(), "file outside grant must not be created");
}

/// Network confinement: with `allow_net = false` the child has no outbound
/// connectivity. An AppContainer with no network capability is blocked from all
/// outbound by the OS firewall — including loopback — so a connect to a
/// parent-owned listener must fail. No admin / WFP required.
#[tokio::test]
async fn net_denied_blocks_outbound() {
    assert!(is_supported(), "windows sandbox must be supported");

    // A bound listener: the TCP handshake would complete from the backlog if the
    // connect were permitted, so a "blocked" result is unambiguous.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let dir = tempfile::tempdir().unwrap();
    let script = format!(
        "(Test-NetConnection -ComputerName 127.0.0.1 -Port {port} \
         -InformationLevel Quiet -WarningAction SilentlyContinue)"
    );
    let res = exec(req(
        powershell(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        policy(dir.path()), // allow_net = false
    ))
    .await
    .unwrap();

    assert_eq!(res.exit_code, 0, "probe should run; stderr: {}", res.stderr);
    assert!(
        !res.stdout.contains("True"),
        "net-denied child reached the loopback listener — AppContainer net block missing; stdout: {:?}",
        res.stdout
    );
}

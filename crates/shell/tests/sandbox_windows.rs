#![cfg(target_os = "windows")]
//! Real restricted-token enforcement tests for the Windows backend.
//!
//! Regression coverage for DLL-init under the sandbox: a child launched with a
//! write-restricted token must still be able to attach to the window station /
//! desktop during `user32`/`gdi32` initialization. Without that, any non-trivial
//! binary (python, powershell, ...) dies with `STATUS_DLL_INIT_FAILED`
//! (`0xC0000142` => `-1073741502`) before `main` ever runs.

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
/// confinement boundary: the RESTRICTED / logon / Everyone restricting SIDs
/// added so binaries can start must not let the child write user files (a temp
/// dir under the profile grants the user SID, not Everyone).
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

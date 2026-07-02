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

use agentd_permissions::Permission;
use agentd_shell::sandbox::is_supported;
use agentd_shell::{ExecRequest, SandboxPolicy, exec};

const STATUS_DLL_INIT_FAILED: i32 = -1073741502; // 0xC0000142

/// Serialize the sandbox tests. They all create AppContainers under one shared
/// profile and mutate the machine-global loopback-exemption list, so running
/// them in parallel races on that global state — and the concurrent per-test
/// runtimes intermittently trip the tokio I/O driver on Windows. One at a time.
static SANDBOX_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn policy(write: &std::path::Path) -> SandboxPolicy {
    SandboxPolicy {
        read_paths: vec![],
        write_paths: vec![write.to_path_buf()],
        allow_net: false,
        net_hosts: vec![],
        unrestricted: false,
    }
}

/// Policy that permits network to exactly the named hosts (host-granular).
fn net_policy(write: &std::path::Path, hosts: &[&str]) -> SandboxPolicy {
    SandboxPolicy {
        read_paths: vec![],
        write_paths: vec![write.to_path_buf()],
        allow_net: true,
        net_hosts: hosts
            .iter()
            .map(|h| Permission::new(format!("net:{h}")))
            .collect(),
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
    let _serial = SANDBOX_SERIAL.lock().await;
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

/// Locate a user-installed `python.exe` on PATH (never under System32), to
/// exercise the child-side DLL load path. `None` if Python is not installed.
fn user_python() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|d| {
        // Skip the Windows Store reparse stub under WindowsApps.
        if d.to_string_lossy().contains("WindowsApps") {
            return None;
        }
        let c = d.join("python.exe");
        c.is_file().then(|| c.to_string_lossy().into_owned())
    })
}

/// A bare executable name that is on `PATH` but NOT under System32 must resolve
/// and run through the sandbox: the child's AppContainer cannot search `PATH`
/// itself (its lowbox token cannot stat those directories), so the daemon must
/// resolve the name to an absolute path before spawning. Regression test for
/// "executable not found" on a name that is plainly on `PATH`.
#[tokio::test]
async fn bare_name_on_path_resolves() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");
    if user_python().is_none() {
        eprintln!("skip: no user python.exe on PATH");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    // Bare name, NOT an absolute path.
    let res = exec(req(
        "python.exe".into(),
        vec!["-c".into(), "print('alive')".into()],
        policy(dir.path()),
    ))
    .await
    .unwrap();

    assert_eq!(res.exit_code, 0, "bare name failed to resolve; stderr: {}", res.stderr);
    assert!(
        res.stdout.contains("alive"),
        "expected child stdout, got: {:?}",
        res.stdout
    );
}

/// A user-installed binary must load the DLLs sitting next to it inside the
/// AppContainer. Its install directory does not grant `ALL_APPLICATION_PACKAGES`
/// (unlike System32), so unless the package SID is granted read+execute there,
/// the child dies at load with `STATUS_DLL_NOT_FOUND` / `STATUS_DLL_INIT_FAILED`
/// before producing any output. Regression test for that grant.
#[tokio::test]
async fn user_installed_binary_loads_its_own_dlls() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");

    let Some(py) = user_python() else {
        eprintln!("no user-installed python on PATH; skipping");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let res = exec(req(py, vec!["--version".into()], policy(dir.path())))
        .await
        .unwrap();

    assert_eq!(
        res.exit_code, 0,
        "user-installed binary failed to load its co-located DLLs under the \
         AppContainer (exit {:#x}); stderr: {}",
        res.exit_code as u32, res.stderr
    );
    assert!(
        res.stdout.to_lowercase().contains("python"),
        "expected version output, got: {:?}",
        res.stdout
    );
}

/// A minimal write inside the granted scratch dir must still succeed: the
/// window-station fix must not loosen the write-restriction confinement.
#[tokio::test]
async fn write_inside_grant_succeeds() {
    let _serial = SANDBOX_SERIAL.lock().await;
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
    let _serial = SANDBOX_SERIAL.lock().await;
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

/// Absolute path to the bundled `curl.exe` (System32). Present on Windows 10
/// 1803+ and the CI runners. Used as a capability-free network probe — unlike
/// `Test-NetConnection`, it needs no PowerShell module to load.
fn curl() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    format!(r"{root}\System32\curl.exe")
}

/// Network confinement: with `allow_net = false` the child has no outbound
/// connectivity. An AppContainer with no network capability is blocked from all
/// outbound by the OS firewall — including loopback — so a connect to a
/// parent-owned responder must fail. No admin / WFP required.
#[tokio::test]
async fn net_denied_blocks_outbound() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");

    // A one-shot HTTP responder: if the child's connect were permitted, curl
    // would reach it and exit 0. Under the net block the connect fails, so curl
    // exits non-zero — an unambiguous "blocked".
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            use std::io::Write;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
        }
    });

    let dir = tempfile::tempdir().unwrap();
    let res = exec(req(
        curl(),
        vec![
            "-s".into(),
            "-m".into(),
            "5".into(),
            "-o".into(),
            "NUL".into(),
            format!("http://127.0.0.1:{port}/"),
        ],
        policy(dir.path()), // allow_net = false
    ))
    .await
    .unwrap();

    assert_ne!(
        res.exit_code, 0,
        "net-denied child reached the loopback responder — AppContainer net block missing; stderr: {}",
        res.stderr
    );
}

/// Host-granular network: with only `example.com` permitted, a request to it
/// succeeds while a request to the non-allowlisted `google.com` is blocked. The
/// child's outbound is default-denied by WFP and permitted only to the resolved
/// IPs of allowed hosts, so the denied case proves it cannot reach an arbitrary
/// host. Behaviour matches Linux/macOS.
#[tokio::test]
async fn net_host_allowlist_is_enforced() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");

    // Requires the broker service (installed via `daemon --install-sandbox`).
    if !agentd_shell::netbroker::available() {
        eprintln!("network broker not installed; skipping");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    // Write the body into the granted write dir (NUL is unwritable under the
    // AppContainer and would surface as a curl write error, not a net result).
    let outfile = dir.path().join("body");
    let curl_args = |url: &str| {
        vec![
            "-sS".into(),
            "-m".into(),
            "15".into(),
            "-o".into(),
            outfile.to_string_lossy().into_owned(),
            url.to_string(),
        ]
    };

    // Allowed host reachable via the proxy.
    let allowed = exec(req(
        curl(),
        curl_args("http://example.com/"),
        net_policy(dir.path(), &["example.com"]),
    ))
    .await
    .unwrap();
    assert_eq!(
        allowed.exit_code, 0,
        "allowlisted host must be reachable; exit={} stderr:\n{}",
        allowed.exit_code, allowed.stderr
    );

    // Non-allowlisted host blocked by the proxy.
    let denied = exec(req(
        curl(),
        curl_args("http://google.com/"),
        net_policy(dir.path(), &["example.com"]),
    ))
    .await
    .unwrap();
    assert_ne!(
        denied.exit_code, 0,
        "non-allowlisted host must be blocked, but curl succeeded"
    );
}

/// Raw SMTP (not HTTP) through the sandbox: with `smtp.gmail.com` granted, a
/// Python `socket` connect to port 587 reaches the server and reads its `220`
/// banner; with only an unrelated host granted, the same connect is blocked.
/// Proves the WFP model is all-protocol — not limited to an HTTP proxy — and
/// still host-granular. Reachability only; sends no mail and needs no creds.
#[tokio::test]
async fn python_smtp_allow_and_deny() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");

    if !agentd_shell::netbroker::available() {
        eprintln!("network broker not installed; skipping");
        return;
    }
    let Some(py) = user_python() else {
        eprintln!("no user-installed python on PATH; skipping");
        return;
    };

    let dir = tempfile::tempdir().unwrap();
    let script = "import socket, sys\n\
try:\n\
\x20   s = socket.create_connection((\"smtp.gmail.com\", 587), timeout=15)\n\
\x20   banner = s.recv(64); s.close()\n\
\x20   print(\"SMTP_OK\" if banner[:3] == b\"220\" else \"SMTP_BAD\")\n\
except Exception as e:\n\
\x20   print(\"SMTP_FAIL\", e); sys.exit(1)\n";
    let script_path = dir.path().join("probe.py");
    std::fs::write(&script_path, script).unwrap();
    let script_arg = script_path.to_string_lossy().into_owned();

    // Allowed: smtp.gmail.com is granted → connect + banner succeed.
    let allow = exec(req(
        py.clone(),
        vec![script_arg.clone()],
        net_policy(dir.path(), &["smtp.gmail.com"]),
    ))
    .await
    .unwrap();
    assert_eq!(
        allow.exit_code, 0,
        "granted SMTP host must be reachable; stdout={:?} stderr={}",
        allow.stdout, allow.stderr
    );
    assert!(
        allow.stdout.contains("SMTP_OK"),
        "expected SMTP banner; got {:?}",
        allow.stdout
    );

    // Denied: only an unrelated host granted → SMTP connect blocked by WFP.
    let deny = exec(req(
        py,
        vec![script_arg],
        net_policy(dir.path(), &["example.com"]),
    ))
    .await
    .unwrap();
    assert_ne!(
        deny.exit_code, 0,
        "non-granted SMTP host must be blocked; stdout={:?}",
        deny.stdout
    );
    assert!(
        !deny.stdout.contains("SMTP_OK"),
        "reached SMTP without a grant: {:?}",
        deny.stdout
    );
}

/// `..` cannot climb out of the granted write subtree.
#[tokio::test]
async fn write_via_parent_traversal_is_denied() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");

    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("pwned.txt");
    // {granted}\..\{outside_name}\pwned.txt resolves to a sibling not granted.
    let escape = format!(
        "{}\\..\\{}\\pwned.txt",
        granted.path().display(),
        outside.path().file_name().unwrap().to_string_lossy()
    );
    let res = exec(req(
        powershell(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!("Set-Content -LiteralPath '{escape}' -Value 'x' -NoNewline"),
        ],
        policy(granted.path()),
    ))
    .await
    .unwrap();
    assert_ne!(res.exit_code, 0, "traversal write must fail");
    assert!(!target.exists(), "file outside grant must not exist");
}

/// A file outside every grant is unreadable by the AppContainer.
#[tokio::test]
async fn read_outside_grant_is_denied() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");

    let granted = tempfile::tempdir().unwrap();
    let secret_dir = tempfile::tempdir().unwrap();
    let secret = secret_dir.path().join("secret.txt");
    std::fs::write(&secret, "TOPSECRET").unwrap();
    let res = exec(req(
        powershell(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!("Get-Content -LiteralPath '{}'", secret.display()),
        ],
        policy(granted.path()),
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

/// A grandchild (the child spawns another process) stays confined: the
/// AppContainer token is inherited across process creation.
#[tokio::test]
async fn grandchild_inherits_filesystem_confinement() {
    let _serial = SANDBOX_SERIAL.lock().await;
    assert!(is_supported(), "windows sandbox must be supported");

    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("pwned.txt");
    // Outer PowerShell spawns an inner PowerShell that attempts the escape.
    let inner = format!(
        "Set-Content -LiteralPath '{}' -Value 'x' -NoNewline",
        target.display()
    );
    let res = exec(req(
        powershell(),
        vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            format!(
                "& '{}' -NoProfile -NonInteractive -Command \"{}\"",
                powershell(),
                inner
            ),
        ],
        policy(granted.path()),
    ))
    .await
    .unwrap();
    assert!(!target.exists(), "grandchild escaped fs confinement");
    let _ = res;
}

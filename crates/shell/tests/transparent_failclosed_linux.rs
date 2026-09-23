#![cfg(target_os = "linux")]
//! Fail-closed setup paths of the transparent netns backend. Each case removes
//! one prerequisite and asserts the exec returns an error instead of running
//! the command. Kept in its own test binary because the cases rewrite `PATH`,
//! which would race the parallel tests in `transparent_linux.rs`.
//!
//! Opt-in with `AGENTD_TEST_NETNS=1`, like the other netns tests.

use agentd_permissions::Permission;
use agentd_shell::sandbox::linux_net::userns_net_supported;
use agentd_shell::{ExecRequest, SandboxPolicy, ShellError};

const INNER_ENV: &str = "AGENTD_TEST_USERNS_DISABLED_INNER";

/// Serializes the tests that read or rewrite `PATH`.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn enabled() -> bool {
    std::env::var("AGENTD_TEST_NETNS").as_deref() == Ok("1")
}

fn marker_req(marker: &std::path::Path) -> ExecRequest {
    ExecRequest {
        bin: "/bin/sh".into(),
        args: vec!["-c".into(), format!("echo ran > '{}'", marker.display())],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(SandboxPolicy {
            read_paths: vec![],
            write_paths: vec![marker.parent().unwrap().to_path_buf()],
            allow_net: true,
            net_hosts: vec![Permission::new("net:203.0.113.77")],
            unrestricted: false,
        }),
    }
}

/// Absolute path of `tool` on the current PATH.
fn which(tool: &str) -> std::path::PathBuf {
    let out = std::process::Command::new("sh")
        .args(["-c", &format!("command -v {tool}")])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().into()
}

/// A directory holding symlinks to exactly `tools`, used as the whole PATH.
fn path_with(tools: &[(&str, &std::path::Path)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, real) in tools {
        std::os::unix::fs::symlink(real, dir.path().join(name)).unwrap();
    }
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_netns_tooling_fails_closed() {
    if !enabled() || !userns_net_supported() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns)");
        return;
    }
    let _guard = ENV_LOCK.lock().await;
    unsafe {
        std::env::set_var(
            "AGENTD_NETNS_SUPERVISOR_BIN",
            env!("CARGO_BIN_EXE_agentd-netns-supervisor"),
        )
    };
    let (ip, nft) = (which("ip"), which("nft"));
    let saved = std::env::var_os("PATH");
    for (tools, missing) in [
        (vec![("ip", ip.as_path())], "nft"),
        (vec![("nft", nft.as_path())], "ip"),
    ] {
        let bin_dir = path_with(&tools);
        unsafe { std::env::set_var("PATH", bin_dir.path()) };
        let scratch = tempfile::tempdir().unwrap();
        let marker = scratch.path().join("marker");
        let res = agentd_shell::exec(marker_req(&marker)).await;
        match res {
            Err(ShellError::Sandbox(msg)) => assert!(
                msg.contains(&format!("`{missing}` command is not installed")),
                "unexpected setup error without {missing}: {msg}"
            ),
            other => panic!("exec without {missing} must fail closed, got {other:?}"),
        }
        assert!(!marker.exists(), "command ran without {missing}");
    }
    if let Some(p) = saved {
        unsafe { std::env::set_var("PATH", p) };
    }
}

/// Re-runs this test inside a user namespace whose `max_user_namespaces` is 0,
/// which is how a host with unprivileged user namespaces disabled looks to the
/// daemon. The inner run must refuse the network-granted command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disabled_user_namespaces_fail_closed() {
    if std::env::var_os(INNER_ENV).is_some() {
        assert!(!userns_net_supported(), "probe must see userns disabled");
        let scratch = tempfile::tempdir().unwrap();
        let marker = scratch.path().join("marker");
        let res = agentd_shell::exec(marker_req(&marker)).await;
        assert!(
            matches!(res, Err(ShellError::NetSandboxUnavailable)),
            "expected NetSandboxUnavailable, got {res:?}"
        );
        assert!(!marker.exists(), "command ran with userns disabled");
        return;
    }
    if !enabled() || !userns_net_supported() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns)");
        return;
    }
    let _guard = ENV_LOCK.lock().await;
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("unshare")
        .args([
            "-r",
            "sh",
            "-c",
            "echo 0 > /proc/sys/user/max_user_namespaces && exec \"$0\" --exact disabled_user_namespaces_fail_closed --nocapture",
        ])
        .arg(&exe)
        .env(INNER_ENV, "1")
        .output();
    let Ok(out) = out else {
        eprintln!("skip: util-linux `unshare` not available");
        return;
    };
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "inner run failed: {text}");
    assert!(text.contains("1 passed"), "inner test did not run: {text}");
}

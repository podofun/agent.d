#![cfg(target_os = "macos")]
//! End-to-end transparent pf-broker backend (macOS): an unmodified binary doing
//! raw TCP is reachable iff the destination IP is granted, DNS resolves only
//! allowed names, and wildcard `net:` grants are honored.
//!
//! These run automatically on macOS whenever the broker is installed and
//! running (`sudo agentd --install-sandbox`); on a machine without it they skip
//! cleanly rather than fail, so a plain `cargo test` is always green.

use agentd_permissions::Permission;
use agentd_shell::{ExecRequest, SandboxPolicy};

fn e2e_enabled() -> bool {
    agentd_shell::sandbox::net_supported()
}

const SKIP: &str = "skip: pf broker not installed (run: sudo agentd --install-sandbox)";

fn policy(net_hosts: Vec<&str>) -> SandboxPolicy {
    SandboxPolicy {
        read_paths: vec![],
        write_paths: vec![],
        allow_net: true,
        net_hosts: net_hosts.into_iter().map(Permission::new).collect(),
        unrestricted: false,
        ..Default::default()
    }
}

/// A literal IP grant lets the child reach exactly that IP and exchange data.
/// Uses a reachable external host (a NAT VM cannot hairpin to its own address).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_tcp_allowed_ip_reaches_server() {
    if !e2e_enabled() {
        eprintln!("{SKIP}");
        return;
    }
    // Connect by literal IP so no name resolution is involved; grant that IP.
    let script = "exec 3<>/dev/tcp/1.1.1.1/80 && printf 'GET / HTTP/1.0\r\nHost: one.one.one.one\r\n\r\n' >&3 && head -c4 <&3";
    let mut req = ExecRequest {
        bin: "/bin/bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        timeout_ms: None,
        sandbox: None,
    };
    req.sandbox = Some(policy(vec!["net:1.1.1.1"]));
    let res = agentd_shell::exec(req).await.expect("exec");
    assert!(
        res.stdout.starts_with("HTTP"),
        "allowed literal IP must reach host + exchange data: {res:?}"
    );
}

/// A connection to an IP no grant covers is dropped by the relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_tcp_denied_ip_is_blocked() {
    if !e2e_enabled() {
        eprintln!("{SKIP}");
        return;
    }
    // Connect to 1.1.1.1 but grant a DIFFERENT literal IP → not admitted.
    let script = "exec 3<>/dev/tcp/1.1.1.1/80 && printf 'GET / HTTP/1.0\r\n\r\n' >&3 && head -c4 <&3 && echo GOT";
    let mut req = ExecRequest {
        bin: "/bin/bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        timeout_ms: None,
        sandbox: None,
    };
    req.sandbox = Some(policy(vec!["net:203.0.113.77"]));
    let res = agentd_shell::exec(req).await.expect("exec");
    assert!(
        !res.stdout.starts_with("HTTP"),
        "denied IP must NOT exchange data: {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dns_denied_name_does_not_resolve() {
    if !e2e_enabled() {
        eprintln!("{SKIP}");
        return;
    }
    // dscacheutil/host would use the system resolver; use bash /dev/tcp to a
    // name — resolution happens through our DNS pin, and a denied name gets
    // NXDOMAIN so the connect fails.
    let script = "exec 3<>/dev/tcp/blocked.example.com/80 && echo REACHED";
    let mut req = ExecRequest {
        bin: "/bin/bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        timeout_ms: None,
        sandbox: None,
    };
    req.sandbox = Some(policy(vec!["net:allowed.example.com"]));
    let res = agentd_shell::exec(req).await.expect("exec");
    assert!(
        !res.stdout.contains("REACHED"),
        "denied name must not resolve: {res:?}"
    );
}

/// Name-grant parity: a host grant (`net:one.one.one.one`) lets the child
/// connect by NAME and exchange data — the child resolves via mDNSResponder
/// (outside our per-uid DNS redirect), and the relay's reactive resolution maps
/// the destination IP back to the allowed name at connect time. Reads the HTTP
/// response so this proves the full data path, not just connect (which succeeds
/// the instant the relay accepts the redirect).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn name_grant_reaches_host_and_exchanges_data() {
    if !e2e_enabled() {
        eprintln!("{SKIP}");
        return;
    }
    let script = "exec 3<>/dev/tcp/one.one.one.one/80 && printf 'GET / HTTP/1.0\\r\\nHost: one.one.one.one\\r\\n\\r\\n' >&3 && head -c4 <&3";
    let mut req = ExecRequest {
        bin: "/bin/bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        timeout_ms: None,
        sandbox: None,
    };
    req.sandbox = Some(policy(vec!["net:one.one.one.one"]));
    let res = agentd_shell::exec(req).await.expect("exec");
    assert!(
        res.stdout.starts_with("HTTP"),
        "name-granted host must resolve + exchange data (Linux parity): {res:?}"
    );
}

/// Wildcard parity: a suffix-wildcard grant (`net:one.one.one.*`) — impossible
/// on the old IP-preresolve path — now works because the child's DNS goes
/// direct (mDNSResponder denied) and hits our interception, which matches the
/// live query name against the wildcard and pins the answer before replying.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wildcard_grant_resolves_like_linux() {
    if !e2e_enabled() {
        eprintln!("{SKIP}");
        return;
    }
    let script = "exec 3<>/dev/tcp/one.one.one.one/80 && printf 'GET / HTTP/1.0\\r\\nHost: one.one.one.one\\r\\n\\r\\n' >&3 && head -c4 <&3";
    let mut req = ExecRequest {
        bin: "/bin/bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        timeout_ms: None,
        sandbox: None,
    };
    req.sandbox = Some(policy(vec!["net:one.one.one.*"]));
    let res = agentd_shell::exec(req).await.expect("exec");
    assert!(
        res.stdout.starts_with("HTTP"),
        "wildcard-granted name must resolve + exchange data (Linux parity): {res:?}"
    );
}

/// A unique `sleep` duration, so `pgrep -f` finds exactly this test's process
/// whatever uid it runs as.
fn marker() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("30.{nanos:09}")
}

/// A script whose background `sleep` (the grandchild) carries `marker`, while
/// the shell itself blocks in a foreground `sleep`.
fn tree_script(marker: &str) -> String {
    format!("sleep {marker} & sleep 30")
}

fn running(marker: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", &format!("sleep {marker}")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn wait_until(marker: &str, want_running: bool) {
    for _ in 0..400 {
        if running(marker) == want_running {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "`sleep {marker}` running = {}, expected {want_running}",
        !want_running
    );
}

fn tree_req(marker: &str, timeout_ms: Option<u64>) -> ExecRequest {
    ExecRequest {
        bin: "/bin/sh".into(),
        args: vec!["-c".into(), tree_script(marker)],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        timeout_ms,
        sandbox: Some(policy(vec!["net:203.0.113.77"])),
    }
}

/// The child runs as a broker-leased uid the daemon cannot signal, so a
/// timeout hangs up on the broker, which kills the child's process group.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timeout_kills_the_broker_spawned_command_and_its_children() {
    if !e2e_enabled() {
        eprintln!("{SKIP}");
        return;
    }
    let marker = marker();
    let err = agentd_shell::exec(tree_req(&marker, Some(3000)))
        .await
        .unwrap_err();
    assert_eq!(err.to_string(), "`/bin/sh` was stopped after 3 s");
    wait_until(&marker, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_exec_future_kills_the_broker_spawned_command() {
    if !e2e_enabled() {
        eprintln!("{SKIP}");
        return;
    }
    let marker = marker();
    let task = tokio::spawn(agentd_shell::exec(tree_req(&marker, None)));
    wait_until(&marker, true).await;
    task.abort();
    let _ = task.await;
    wait_until(&marker, false).await;
}

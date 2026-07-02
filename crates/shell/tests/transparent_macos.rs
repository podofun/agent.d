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
        sandbox: None,
    };
    req.sandbox = Some(policy(vec!["net:one.one.one.*"]));
    let res = agentd_shell::exec(req).await.expect("exec");
    assert!(
        res.stdout.starts_with("HTTP"),
        "wildcard-granted name must resolve + exchange data (Linux parity): {res:?}"
    );
}

#![cfg(target_os = "linux")]
//! End-to-end transparent netns backend (Architecture B): an unmodified binary
//! doing **raw TCP** (bash /dev/tcp — no proxy awareness, not HTTP) is reachable
//! iff the destination IP is granted, and DNS resolves only allowed names.
//! Skipped when the host lacks unprivileged user namespaces or `nft`/`ip`.

use std::net::{IpAddr, SocketAddr, UdpSocket};

use agentd_permissions::Permission;
use agentd_shell::sandbox::linux_net::userns_net_supported;
use agentd_shell::{ExecRequest, SandboxPolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// These tests spawn real network namespaces + nft + child processes, which is
/// heavy and privilege-dependent. They are opt-in (set `AGENTD_TEST_NETNS=1`) so
/// a plain `cargo test` never spawns netns machinery (and can't stall a machine
/// by running many in parallel). CI sets the env explicitly.
fn netns_e2e_enabled() -> bool {
    std::env::var("AGENTD_TEST_NETNS").as_deref() == Ok("1")
}

fn supervisor_env() {
    unsafe {
        std::env::set_var(
            "AGENTD_NETNS_SUPERVISOR_BIN",
            env!("CARGO_BIN_EXE_agentd-netns-supervisor"),
        )
    };
}

/// Tooling present? (nft + ip are shelled by the transparent supervisor.)
fn tooling_present() -> bool {
    let runs = |b: &str, flag: &str| {
        std::process::Command::new(b)
            .arg(flag)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    runs("nft", "--version") && runs("ip", "-V")
}

/// The host's primary non-loopback IPv4, discovered via a UDP connect (no packet
/// sent). The child will target this; the host relay connects back to it.
fn host_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:53").ok()?;
    let ip = s.local_addr().ok()?.ip();
    if ip.is_loopback() { None } else { Some(ip) }
}

fn policy(net_hosts: Vec<&str>) -> SandboxPolicy {
    SandboxPolicy {
        read_paths: vec![],
        write_paths: vec![],
        allow_net: true,
        net_hosts: net_hosts.into_iter().map(Permission::new).collect(),
        unrestricted: false,
    }
}

/// A TCP server bound on all interfaces that replies "PONG" to any connection.
async fn pong_server() -> u16 {
    let l = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut b = [0u8; 64];
                let _ = s.read(&mut b).await;
                let _ = s.write_all(b"PONG").await;
            });
        }
    });
    port
}

fn raw_tcp_req(dst: SocketAddr) -> ExecRequest {
    // bash /dev/tcp: raw TCP, no HTTP, no proxy env honored. The exact case the
    // SNI/HTTP proxy could not handle.
    let script = format!(
        "exec 3<>/dev/tcp/{}/{} && printf ping >&3 && head -c4 <&3",
        dst.ip(),
        dst.port()
    );
    ExecRequest {
        // `timeout` guards against a relay hang so a failure surfaces fast.
        bin: "/usr/bin/timeout".into(),
        args: vec!["5".into(), "/bin/bash".into(), "-c".into(), script],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_tcp_allowed_ip_reaches_server() {
    if !netns_e2e_enabled() || !userns_net_supported() || !tooling_present() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns + nft/ip)");
        return;
    }
    let Some(ip) = host_ip() else {
        eprintln!("skip: no non-loopback host IP");
        return;
    };
    supervisor_env();
    let port = pong_server().await;
    let dst = SocketAddr::new(ip, port);

    let mut req = raw_tcp_req(dst);
    // Literal IP grant for the server → admitted.
    req.sandbox = Some(policy(vec![&format!("net:{ip}")]));

    let res = agentd_shell::exec(req).await.expect("exec");
    assert_eq!(
        res.stdout.trim(),
        "PONG",
        "allowed raw-TCP must reach server: {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn raw_tcp_denied_ip_is_blocked() {
    if !netns_e2e_enabled() || !userns_net_supported() || !tooling_present() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns + nft/ip)");
        return;
    }
    let Some(ip) = host_ip() else {
        eprintln!("skip: no non-loopback host IP");
        return;
    };
    supervisor_env();
    let port = pong_server().await;
    let dst = SocketAddr::new(ip, port);

    let mut req = raw_tcp_req(dst);
    // Grant a DIFFERENT IP; the server's IP is NOT permitted → relay drops it.
    req.sandbox = Some(policy(vec!["net:203.0.113.77"]));

    let res = agentd_shell::exec(req).await.expect("exec");
    assert_ne!(
        res.stdout.trim(),
        "PONG",
        "denied IP must NOT reach server: {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dns_denied_name_does_not_resolve() {
    if !netns_e2e_enabled() || !userns_net_supported() || !tooling_present() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns + nft/ip)");
        return;
    }
    supervisor_env();
    // getent hosts <name> uses the system resolver → our intercept. A name no
    // grant covers must NXDOMAIN (getent prints nothing, exits nonzero).
    let mut req = ExecRequest {
        bin: "/usr/bin/getent".into(),
        args: vec!["hosts".into(), "blocked.example.com".into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: None,
    };
    req.sandbox = Some(policy(vec!["net:allowed.example.com"]));
    let res = agentd_shell::exec(req).await.expect("exec");
    assert!(
        !res.stdout.contains("blocked.example.com") && res.exit_code != 0,
        "denied name must not resolve: {res:?}"
    );
}

fn sh_req(script: &str, net_hosts: Vec<&str>) -> ExecRequest {
    ExecRequest {
        bin: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(policy(net_hosts)),
    }
}

/// The command runs as root of the nested user namespace; it must hold no
/// capabilities there, so it cannot rewrite the supervisor's nft redirect or
/// reconfigure the namespace's interfaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn net_child_holds_no_capabilities_and_cannot_touch_nft() {
    if !netns_e2e_enabled() || !userns_net_supported() || !tooling_present() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns + nft/ip)");
        return;
    }
    supervisor_env();
    let res = agentd_shell::exec(sh_req(
        // Builtins only: /proc/self is granted to the exec'd pid, not to
        // grandchildren like grep.
        "while read -r k v; do case $k in CapPrm:|CapEff:|CapBnd:|CapAmb:) echo \"$k $v\";; esac; done < /proc/self/status; \
         if nft flush ruleset 2>/dev/null; then echo NFT_FLUSHED; fi; \
         if ip link set lo down 2>/dev/null; then echo LO_DOWN; fi",
        vec!["net:203.0.113.77"],
    ))
    .await
    .expect("exec");
    for line in res.stdout.lines() {
        assert!(
            line.ends_with("0000000000000000"),
            "capability left in the sandboxed child: {res:?}"
        );
    }
    assert_eq!(res.stdout.lines().count(), 4, "{res:?}");
    assert!(!res.stdout.contains("NFT_FLUSHED"), "{res:?}");
    assert!(!res.stdout.contains("LO_DOWN"), "{res:?}");
}

/// A UDP echo server bound on all interfaces; returns its port.
fn udp_echo_server() -> u16 {
    let s = UdpSocket::bind("0.0.0.0:0").unwrap();
    let port = s.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut b = [0u8; 1500];
        while let Ok((n, peer)) = s.recv_from(&mut b) {
            let _ = s.send_to(&b[..n], peer);
        }
    });
    port
}

/// bash `/dev/udp` opens a *connected* UDP socket, as a QUIC client does, so
/// the echo only arrives if the reply's source is rewritten back to the
/// original destination. `dd` reads one whole datagram; bash `read` would take
/// a single byte and drop the rest.
fn udp_echo_req(dst: SocketAddr, net_hosts: Vec<&str>) -> ExecRequest {
    let script = format!(
        "exec 3<>/dev/udp/{}/{} && printf ping >&3 && echo \"got:$(timeout 3 dd bs=512 count=1 status=none <&3)\"",
        dst.ip(),
        dst.port()
    );
    ExecRequest {
        bin: "/bin/bash".into(),
        args: vec!["-c".into(), script],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: Some(policy(net_hosts)),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_to_allowed_ip_round_trips() {
    if !netns_e2e_enabled() || !userns_net_supported() || !tooling_present() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns + nft/ip)");
        return;
    }
    let Some(ip) = host_ip() else {
        eprintln!("skip: no non-loopback host IP");
        return;
    };
    supervisor_env();
    let dst = SocketAddr::new(ip, udp_echo_server());
    let res = agentd_shell::exec(udp_echo_req(dst, vec![&format!("net:{ip}")]))
        .await
        .expect("exec");
    assert_eq!(
        res.stdout.trim(),
        "got:ping",
        "allowed UDP must round-trip: {res:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_to_denied_ip_is_dropped() {
    if !netns_e2e_enabled() || !userns_net_supported() || !tooling_present() {
        eprintln!("skip: set AGENTD_TEST_NETNS=1 (needs userns + nft/ip)");
        return;
    }
    let Some(ip) = host_ip() else {
        eprintln!("skip: no non-loopback host IP");
        return;
    };
    supervisor_env();
    let dst = SocketAddr::new(ip, udp_echo_server());
    let res = agentd_shell::exec(udp_echo_req(dst, vec!["net:203.0.113.77"]))
        .await
        .expect("exec");
    assert_eq!(
        res.stdout.trim(),
        "got:",
        "denied UDP must not arrive: {res:?}"
    );
}

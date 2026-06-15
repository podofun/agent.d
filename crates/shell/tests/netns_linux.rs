#![cfg(target_os = "linux")]
//! Rootless netns network containment, exercised end-to-end. Skipped when the
//! host has no unprivileged user namespaces (e.g. hardened CI).

use agentd_permissions::Permission;
use agentd_shell::proxy::Proxy;
use agentd_shell::sandbox::linux_net::{run_contained, userns_net_supported};
use agentd_shell::{ExecRequest, SandboxPolicy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn supervisor_env() {
    // Point run_contained at the freshly built supervisor binary.
    unsafe {
        std::env::set_var(
            "AGENTD_NETNS_SUPERVISOR_BIN",
            env!("CARGO_BIN_EXE_agentd-netns-supervisor"),
        )
    };
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

fn req(bin: &str, script: &str) -> ExecRequest {
    ExecRequest {
        bin: bin.into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        stdin: None,
        separate_stderr: true,
        sandbox: None,
    }
}

async fn echo_server() -> u16 {
    let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut b = [0u8; 256];
                while let Ok(n) = s.read(&mut b).await {
                    if n == 0 || s.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

#[tokio::test]
async fn echo_plumbing_and_exit_code() {
    if !userns_net_supported() {
        eprintln!("userns unavailable; skipping");
        return;
    }
    supervisor_env();
    let proxy = Proxy::start(vec![]).await.unwrap();
    let r = run_contained(&req("/bin/echo", "ignored"), &policy(vec![]), &proxy).await;
    // /bin/echo ignores -c; just assert the chain runs and returns.
    let r = r.unwrap();
    assert_eq!(r.exit_code, 0, "stderr: {}", r.stderr);
}

#[tokio::test]
async fn direct_connect_to_public_ip_denied() {
    if !userns_net_supported() {
        eprintln!("userns unavailable; skipping");
        return;
    }
    supervisor_env();
    let proxy = Proxy::start(vec![]).await.unwrap();
    // No route in the netns → connecting to a non-loopback address fails.
    let script = "exec 3<>/dev/tcp/8.8.8.8/53 && echo CONNECTED || echo BLOCKED";
    let r = run_contained(&req("/bin/bash", script), &policy(vec![]), &proxy)
        .await
        .unwrap();
    assert!(
        r.stdout.contains("BLOCKED"),
        "direct egress must be blocked; got stdout={:?} stderr={:?}",
        r.stdout,
        r.stderr
    );
}

#[tokio::test]
async fn proxied_allowed_host_round_trips() {
    if !userns_net_supported() {
        eprintln!("userns unavailable; skipping");
        return;
    }
    supervisor_env();
    let upstream = echo_server().await;
    unsafe { std::env::set_var("AGENTD_TEST_UPSTREAM", upstream.to_string()) };
    let proxy = Proxy::start(vec![Permission::new("net:127.0.0.1")])
        .await
        .unwrap();

    // Connect to the in-netns proxy port (from $HTTP_PROXY), CONNECT-tunnel to the
    // allowed loopback upstream, then echo a token through.
    let script = r#"
        port="${HTTP_PROXY##*:}"
        exec 3<>/dev/tcp/127.0.0.1/"$port" || { echo NOPROXY; exit 1; }
        printf 'CONNECT 127.0.0.1:%s HTTP/1.1\r\n\r\n' "$AGENTD_TEST_UPSTREAM" >&3
        # drain the proxy's 200 response headers up to the blank line
        while IFS= read -r line <&3; do
            line=${line%$'\r'}
            [ -z "$line" ] && break
        done
        printf 'PONGME' >&3
        head -c 6 <&3
    "#;
    let r = run_contained(
        &req("/bin/bash", script),
        &policy(vec!["net:127.0.0.1"]),
        &proxy,
    )
    .await
    .unwrap();
    assert!(
        r.stdout.contains("PONGME"),
        "expected tunneled echo through allowed host; got stdout={:?} stderr={:?}",
        r.stdout,
        r.stderr
    );
}

#[tokio::test]
async fn proxied_disallowed_host_denied() {
    if !userns_net_supported() {
        eprintln!("userns unavailable; skipping");
        return;
    }
    supervisor_env();
    let upstream = echo_server().await;
    unsafe { std::env::set_var("AGENTD_TEST_UPSTREAM", upstream.to_string()) };
    // Allow only a DIFFERENT host; the CONNECT to loopback must be denied by the
    // proxy (it closes), so no 200 arrives and PONGME never round-trips.
    let proxy = Proxy::start(vec![Permission::new("net:example.com")])
        .await
        .unwrap();
    let script = r#"
        port="${HTTP_PROXY##*:}"
        exec 3<>/dev/tcp/127.0.0.1/"$port" || { echo NOPROXY; exit 1; }
        printf 'CONNECT 127.0.0.1:%s HTTP/1.1\r\n\r\n' "$AGENTD_TEST_UPSTREAM" >&3
        while IFS= read -r line <&3; do
            line=${line%$'\r'}
            [ -z "$line" ] && break
        done
        printf 'PONGME' >&3
        head -c 6 <&3
        echo DONE
    "#;
    let r = run_contained(
        &req("/bin/bash", script),
        &policy(vec!["net:example.com"]),
        &proxy,
    )
    .await
    .unwrap();
    assert!(
        !r.stdout.contains("PONGME"),
        "disallowed host must not round-trip; got stdout={:?}",
        r.stdout
    );
}

#[tokio::test]
async fn net_off_blocks_all_network() {
    if !userns_net_supported() {
        eprintln!("userns unavailable; skipping");
        return;
    }
    supervisor_env();
    // allow_net = false routes through the Phase 1 Landlock path, which denies
    // all TCP. Even a loopback connect must fail.
    let upstream = echo_server().await;
    let p = SandboxPolicy {
        read_paths: vec![],
        write_paths: vec![],
        allow_net: false,
        net_hosts: vec![],
        unrestricted: false,
    };
    let script = format!("exec 3<>/dev/tcp/127.0.0.1/{upstream} && echo OPEN || echo BLOCKED");
    let mut request = req("/bin/bash", &script);
    request.sandbox = Some(p);
    let r = agentd_shell::exec(request).await.unwrap();
    assert!(
        r.stdout.contains("BLOCKED"),
        "net-off must block all network; got stdout={:?} stderr={:?}",
        r.stdout,
        r.stderr
    );
}

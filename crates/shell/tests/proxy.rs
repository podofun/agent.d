//! Host-side proxy behavior over real loopback TCP with a fake upstream.

use agentd_permissions::Permission;
use agentd_shell::proxy::Proxy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Spawn a loopback echo server; return its address.
async fn echo_server() -> std::net::SocketAddr {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sock.write_all(&buf[..n]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

async fn connect_via_proxy(proxy: &Proxy, target: &str) -> TcpStream {
    let mut c = TcpStream::connect(proxy.addr()).await.unwrap();
    let req = format!("CONNECT {target} HTTP/1.1\r\n\r\n");
    c.write_all(req.as_bytes()).await.unwrap();
    c
}

#[tokio::test]
async fn allows_listed_host_relays() {
    let upstream = echo_server().await;
    let proxy = Proxy::start(vec![Permission::new("net:127.0.0.1")])
        .await
        .unwrap();

    let mut c = connect_via_proxy(&proxy, &format!("127.0.0.1:{}", upstream.port())).await;

    // Read the 200 tunnel response.
    let mut head = [0u8; 39];
    c.read_exact(&mut head).await.unwrap();
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "expected tunnel established, got: {:?}",
        String::from_utf8_lossy(&head)
    );

    // Now the tunnel is to the echo server: ping → pong.
    c.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
}

#[tokio::test]
async fn denies_unlisted_host() {
    let upstream = echo_server().await;
    // Allow only a DIFFERENT host; the loopback CONNECT must be refused.
    let proxy = Proxy::start(vec![Permission::new("net:example.com")])
        .await
        .unwrap();

    let mut c = connect_via_proxy(&proxy, &format!("127.0.0.1:{}", upstream.port())).await;

    // The proxy denies by closing; no 200 ever arrives → read returns 0.
    let mut buf = [0u8; 16];
    let n = c.read(&mut buf).await.unwrap();
    assert_eq!(n, 0, "denied connection must be closed, not tunneled");
}

#[tokio::test]
async fn wildcard_grant_allows() {
    let upstream = echo_server().await;
    let proxy = Proxy::start(vec![Permission::new("net:*")]).await.unwrap();
    let mut c = connect_via_proxy(&proxy, &format!("127.0.0.1:{}", upstream.port())).await;
    let mut head = [0u8; 12];
    c.read_exact(&mut head).await.unwrap();
    assert!(head.starts_with(b"HTTP/1.1 200"));
}

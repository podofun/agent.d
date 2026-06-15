//! Per-connection relay: peek → host extraction → `covers` check → upstream
//! connect → bidirectional splice. All host-match policy lives here; the netns
//! supervisor and the control socketpair are dumb plumbing that feed connections
//! into this logic.

use std::sync::Arc;
use std::time::Duration;

use agentd_permissions::Permission;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use super::host::{HostDecision, extract_host};

/// Max bytes buffered while trying to determine the host before giving up.
const MAX_PEEK: usize = 16 * 1024;
/// Deadline for accumulating enough bytes to decide the host.
const PEEK_TIMEOUT: Duration = Duration::from_secs(5);

/// Handle one client connection. `net_hosts` is the allow set (raw `net:<host>`
/// slugs). On deny or any error the connection is simply closed. Never panics.
pub async fn handle_conn(mut client: TcpStream, net_hosts: Arc<Vec<Permission>>) {
    let _ = relay(&mut client, net_hosts).await;
}

async fn relay(client: &mut TcpStream, net_hosts: Arc<Vec<Permission>>) -> std::io::Result<()> {
    // Accumulate bytes until the host can be decided (or denied / timed out).
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    let host = loop {
        match extract_host(&buf) {
            HostDecision::Allow(h) => break h,
            HostDecision::Deny(_) => return Ok(()), // deny: drop the connection
            HostDecision::NeedMore => {
                if buf.len() >= MAX_PEEK {
                    return Ok(()); // too much without a decision → deny
                }
                let n = match timeout(PEEK_TIMEOUT, client.read(&mut tmp)).await {
                    Ok(Ok(n)) => n,
                    _ => return Ok(()), // timeout or read error → deny
                };
                if n == 0 {
                    return Ok(()); // EOF before a decision → deny
                }
                buf.extend_from_slice(&tmp[..n]);
            }
        }
    };

    // Host-match against the allow set, using the exact ctx.http semantics.
    let want = Permission::new(format!("net:{host}"));
    if !net_hosts.iter().any(|h| h.covers(&want)) {
        return Ok(()); // not allowed → drop
    }

    let is_connect = buf.starts_with(b"CONNECT ");
    let port = upstream_port(&buf, is_connect);

    let mut upstream = match TcpStream::connect((host.as_str(), port)).await {
        Ok(s) => s,
        Err(_) => return Ok(()), // upstream unreachable → close (child sees failure)
    };

    if is_connect {
        // Establish the tunnel; do NOT forward the CONNECT request line upstream.
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
    } else {
        // Replay the already-read bytes (TLS ClientHello or HTTP request) upstream.
        upstream.write_all(&buf).await?;
    }

    tokio::io::copy_bidirectional(client, &mut upstream).await?;
    Ok(())
}

/// Decide the upstream port: CONNECT authority's port, else 443 for TLS, 80 for
/// HTTP.
fn upstream_port(buf: &[u8], is_connect: bool) -> u16 {
    if is_connect {
        if let Some(p) = connect_port(buf) {
            return p;
        }
        return 443;
    }
    if buf.first() == Some(&0x16) { 443 } else { 80 }
}

/// Parse the port from a `CONNECT host:port HTTP/1.1` request line.
fn connect_port(buf: &[u8]) -> Option<u16> {
    let rest = buf.strip_prefix(b"CONNECT ")?;
    let end = rest.iter().position(|&b| b == b' ')?;
    let authority = std::str::from_utf8(&rest[..end]).ok()?.trim();
    // Strip IPv6 brackets if present.
    let after_host = if let Some(idx) = authority.rfind(']') {
        &authority[idx + 1..]
    } else {
        authority
    };
    let port = after_host.rsplit(':').next()?;
    port.parse::<u16>().ok()
}

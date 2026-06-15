//! In-process egress proxy for sandboxed shell children.
//!
//! The proxy is the only network path out of a sandboxed child. It reads the
//! destination host from the peeked client bytes (`host::extract_host`), checks
//! it against the policy's allowed `net:<host>` slugs with `Permission::covers`,
//! then relays or denies. No TLS termination, no MITM.

pub mod host;
pub mod relay;

use std::net::SocketAddr;
use std::sync::Arc;

use agentd_permissions::Permission;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// A running egress proxy bound to a loopback port. Dropping it shuts the
/// accept loop down.
pub struct Proxy {
    addr: SocketAddr,
    // Dropping the sender signals the accept loop to stop.
    _shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Proxy {
    /// Bind `127.0.0.1:0` and spawn the accept loop. `net_hosts` is the allow
    /// set of raw `net:<host>` grant slugs.
    pub async fn start(net_hosts: Vec<Permission>) -> std::io::Result<Proxy> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let hosts = Arc::new(net_hosts);
        let (tx, mut rx) = oneshot::channel::<()>();

        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut rx => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _)) => {
                                let hosts = hosts.clone();
                                tokio::spawn(relay::handle_conn(stream, hosts));
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Ok(Proxy {
            addr,
            _shutdown: tx,
            handle,
        })
    }

    /// The loopback address the proxy listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

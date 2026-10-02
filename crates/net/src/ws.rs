use std::time::Duration;

use thiserror::Error;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub use tokio_tungstenite::tungstenite;

/// Parse the host out of a ws:// or wss:// URL. Useful for callers building
/// the `net:<host>` permission slug.
pub fn host_of(url: &str) -> Result<String, WsError> {
    let parsed =
        url::Url::parse(url).map_err(|e| WsError::InvalidUrl(url.to_string(), e.to_string()))?;
    parsed
        .host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| WsError::InvalidUrl(url.to_string(), "no host".into()))
}

#[derive(Debug, Error)]
pub enum WsError {
    #[error("the URL `{0}` is not valid ({1})")]
    InvalidUrl(String, String),
    #[error("the WebSocket handshake failed ({0})")]
    Handshake(String),
    #[error("I/O error on the WebSocket connection ({0})")]
    Io(String),
    #[error("the WebSocket connection is closed")]
    Closed,
    #[error("the WebSocket operation timed out after {}ms", .0.as_millis())]
    Timeout(Duration),
    #[error("`{host}` resolves only to addresses that are not allowed ({reason})")]
    AddressRefused { host: String, reason: String },
}

#[derive(Debug, Clone)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
    /// Close frame. `code` is the WebSocket close status (`1000` clean,
    /// `4xxx` app-defined like Discord's `4014` "disallowed intents").
    /// `reason` is the peer's UTF-8 description, may be empty.
    Close {
        code: u16,
        reason: String,
    },
}

/// Owns the WebSocket stream behind a Mutex so multiple Lua handler calls can
/// share a handle safely. Each `send` / `recv` is one round trip.
pub struct Connection {
    inner: Mutex<Inner>,
    url: String,
    /// The address the connection reached, when it was opened with
    /// [`Connection::connect_checked`].
    peer_ip: Option<std::net::IpAddr>,
}

type Stream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Inner {
    stream: Option<Stream>,
}

impl Connection {
    pub async fn connect(url: &str) -> Result<Self, WsError> {
        let (stream, _resp) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|e| WsError::Handshake(e.to_string()))?;
        Ok(Self {
            inner: Mutex::new(Inner {
                stream: Some(stream),
            }),
            url: url.to_string(),
            peer_ip: None,
        })
    }

    /// Like [`Connection::connect`], but the host name is resolved here and
    /// every address must pass `check` before a TCP connection is opened to
    /// it, so a name that resolves only to refused addresses is never
    /// contacted. The handshake (and TLS, for `wss://`) then runs over that
    /// connection with the URL's own host name. An IP literal in the URL is
    /// not resolved, so the caller checks it with the URL itself.
    pub async fn connect_checked(
        url: &str,
        check: crate::http::AddressCheck,
    ) -> Result<Self, WsError> {
        let parsed =
            url::Url::parse(url).map_err(|e| WsError::InvalidUrl(url.into(), e.to_string()))?;
        let host = host_of(url)?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| WsError::InvalidUrl(url.into(), "no port".into()))?;
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        let found: Vec<std::net::SocketAddr> = tokio::net::lookup_host((bare, port))
            .await
            .map_err(|e| WsError::Handshake(e.to_string()))?
            .collect();
        let mut last_reason = None;
        let allowed: Vec<std::net::SocketAddr> = found
            .into_iter()
            .filter(|a| match check(a.ip().to_canonical()) {
                Ok(()) => true,
                Err(reason) => {
                    last_reason = Some(reason);
                    false
                }
            })
            .collect();
        if allowed.is_empty() {
            return Err(match last_reason {
                Some(reason) => WsError::AddressRefused { host, reason },
                None => WsError::Handshake(format!("`{host}` did not resolve to any address")),
            });
        }
        let tcp = tokio::net::TcpStream::connect(allowed.as_slice())
            .await
            .map_err(|e| WsError::Handshake(e.to_string()))?;
        let peer_ip = tcp.peer_addr().ok().map(|a| a.ip().to_canonical());
        let (stream, _resp) = tokio_tungstenite::client_async_tls(url, tcp)
            .await
            .map_err(|e| WsError::Handshake(e.to_string()))?;
        Ok(Self {
            inner: Mutex::new(Inner {
                stream: Some(stream),
            }),
            url: url.to_string(),
            peer_ip,
        })
    }

    /// The address this connection reached, if it was opened with
    /// [`Connection::connect_checked`].
    pub fn peer_ip(&self) -> Option<std::net::IpAddr> {
        self.peer_ip
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub async fn send_text(&self, text: &str) -> Result<(), WsError> {
        use futures_util::SinkExt;
        let mut guard = self.inner.lock().await;
        let s = guard.stream.as_mut().ok_or(WsError::Closed)?;
        s.send(WsMessage::Text(text.to_string().into()))
            .await
            .map_err(|e| WsError::Io(e.to_string()))
    }

    pub async fn send_binary(&self, bytes: Vec<u8>) -> Result<(), WsError> {
        use futures_util::SinkExt;
        let mut guard = self.inner.lock().await;
        let s = guard.stream.as_mut().ok_or(WsError::Closed)?;
        s.send(WsMessage::Binary(bytes.into()))
            .await
            .map_err(|e| WsError::Io(e.to_string()))
    }

    pub async fn recv(&self, timeout: Option<Duration>) -> Result<Frame, WsError> {
        use futures_util::StreamExt;
        let mut guard = self.inner.lock().await;
        let s = guard.stream.as_mut().ok_or(WsError::Closed)?;
        let fut = s.next();
        let item = match timeout {
            Some(d) => match tokio::time::timeout(d, fut).await {
                Ok(opt) => opt,
                Err(_) => return Err(WsError::Timeout(d)),
            },
            None => fut.await,
        };
        match item {
            None => Err(WsError::Closed),
            Some(Err(e)) => Err(WsError::Io(e.to_string())),
            Some(Ok(msg)) => match msg {
                WsMessage::Text(t) => Ok(Frame::Text(t.to_string())),
                WsMessage::Binary(b) => Ok(Frame::Binary(b.to_vec())),
                WsMessage::Close(cf) => {
                    let (code, reason) = match cf {
                        Some(c) => (u16::from(c.code), c.reason.to_string()),
                        None => (1006, String::new()),
                    };
                    Ok(Frame::Close { code, reason })
                }
                WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {
                    // Suppress control frames; tungstenite handles ping/pong automatically.
                    Ok(Frame::Binary(Vec::new()))
                }
            },
        }
    }

    pub async fn close(&self) -> Result<(), WsError> {
        use futures_util::StreamExt;
        let mut guard = self.inner.lock().await;
        if let Some(mut s) = guard.stream.take() {
            let _ = s.close(None).await;
            while let Some(_msg) = s.next().await {}
        }
        Ok(())
    }

    pub async fn is_closed(&self) -> bool {
        let guard = self.inner.lock().await;
        guard.stream.is_none()
    }
}

#[cfg(test)]
mod tests {
    //! Live WebSocket tests against a local tungstenite echo server.

    use std::net::SocketAddr;
    use std::time::Duration;

    use super::{Connection, Frame, WsError, host_of};

    async fn spawn_echo() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                tokio::spawn(async move {
                    use futures_util::{SinkExt, StreamExt};
                    let ws = match tokio_tungstenite::accept_async(stream).await {
                        Ok(ws) => ws,
                        Err(_) => return,
                    };
                    let (mut tx, mut rx) = ws.split();
                    while let Some(Ok(msg)) = rx.next().await {
                        use super::tungstenite::Message;
                        match msg {
                            Message::Text(_) | Message::Binary(_) => {
                                let _ = tx.send(msg).await;
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn host_extraction() {
        assert_eq!(host_of("ws://localhost:7777/x").unwrap(), "localhost");
        assert_eq!(
            host_of("wss://api.example.com/feed").unwrap(),
            "api.example.com"
        );
        assert!(host_of("not a url").is_err());
    }

    #[tokio::test]
    async fn text_roundtrip() {
        let addr = spawn_echo().await;
        let url = format!("ws://{addr}/");
        let c = Connection::connect(&url).await.unwrap();
        c.send_text("hello").await.unwrap();
        let frame = c.recv(None).await.unwrap();
        match frame {
            Frame::Text(s) => assert_eq!(s, "hello"),
            other => panic!("expected text, got {other:?}"),
        }
        c.close().await.unwrap();
    }

    #[tokio::test]
    async fn binary_roundtrip() {
        let addr = spawn_echo().await;
        let url = format!("ws://{addr}/");
        let c = Connection::connect(&url).await.unwrap();
        c.send_binary(vec![1, 2, 3, 4]).await.unwrap();
        let frame = c.recv(None).await.unwrap();
        match frame {
            Frame::Binary(b) => assert_eq!(b, vec![1, 2, 3, 4]),
            other => panic!("expected binary, got {other:?}"),
        }
        c.close().await.unwrap();
    }

    #[tokio::test]
    async fn recv_times_out() {
        let addr = spawn_echo().await;
        let url = format!("ws://{addr}/");
        let c = Connection::connect(&url).await.unwrap();
        // Don't send; server is silent.
        let err = c.recv(Some(Duration::from_millis(100))).await.unwrap_err();
        assert!(matches!(err, WsError::Timeout(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn close_marks_handle_closed() {
        let addr = spawn_echo().await;
        let url = format!("ws://{addr}/");
        let c = Connection::connect(&url).await.unwrap();
        assert!(!c.is_closed().await);
        c.close().await.unwrap();
        assert!(c.is_closed().await);
        let err = c.send_text("x").await.unwrap_err();
        assert!(matches!(err, WsError::Closed));
    }

    #[tokio::test]
    async fn connect_to_nowhere_is_handshake_error() {
        let err = match Connection::connect("ws://127.0.0.1:1/").await {
            Ok(_) => panic!("expected handshake error"),
            Err(e) => e,
        };
        assert!(
            matches!(err, WsError::Handshake(_) | WsError::Io(_)),
            "got {err:?}"
        );
    }

    fn refuse_loopback() -> crate::http::AddressCheck {
        std::sync::Arc::new(|ip: std::net::IpAddr| {
            if ip.is_loopback() {
                Err(format!("`net:{ip}` is denied"))
            } else {
                Ok(())
            }
        })
    }

    #[tokio::test]
    async fn a_name_resolving_only_to_refused_addresses_is_never_connected() {
        let addr = spawn_echo().await;
        let url = format!("ws://localhost:{}/", addr.port());
        match Connection::connect_checked(&url, refuse_loopback()).await {
            Err(WsError::AddressRefused { host, reason }) => {
                assert_eq!(host, "localhost");
                assert!(reason.contains("denied"), "{reason}");
            }
            Err(other) => panic!("expected AddressRefused, got {other}"),
            Ok(_) => panic!("expected AddressRefused, got a connection"),
        }
    }

    #[tokio::test]
    async fn a_checked_connection_records_the_address_it_reached() {
        let addr = spawn_echo().await;
        let url = format!("ws://localhost:{}/", addr.port());
        let c = Connection::connect_checked(&url, std::sync::Arc::new(|_| Ok(())))
            .await
            .unwrap();
        assert_eq!(c.peer_ip(), Some(addr.ip()));
        c.send_text("hi").await.unwrap();
        assert!(
            matches!(c.recv(Some(Duration::from_secs(5))).await.unwrap(), Frame::Text(t) if t == "hi")
        );
    }
}

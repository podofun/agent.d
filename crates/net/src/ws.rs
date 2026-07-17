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
    #[error("invalid url `{0}`: {1}")]
    InvalidUrl(String, String),
    #[error("handshake: {0}")]
    Handshake(String),
    #[error("io: {0}")]
    Io(String),
    #[error("closed")]
    Closed,
    #[error("timeout after {}ms", .0.as_millis())]
    Timeout(Duration),
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
        })
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
}

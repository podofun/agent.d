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
    #[error("timeout after {0:?}")]
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

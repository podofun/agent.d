//! Live WebSocket tests against a local tungstenite echo server.

use std::net::SocketAddr;
use std::time::Duration;

use agentd_ws::{Connection, Frame, WsError, host_of};

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
                    use agentd_ws::tungstenite::Message;
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

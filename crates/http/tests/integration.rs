//! Live HTTP integration tests against a tiny local hyper server.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentd_http::{Request, send};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper::{Method, Response as HRes, StatusCode};
use hyper_util::rt::TokioIo;

// Full record of a request the echo server saw. Not every test asserts every
// field, so some go unread depending on the case under test.
#[allow(dead_code)]
#[derive(Debug, Default, Clone)]
struct Captured {
    method: String,
    path: String,
    body: String,
    headers: BTreeMap<String, String>,
}

async fn spawn_echo() -> (SocketAddr, Arc<Mutex<Vec<Captured>>>) {
    let log: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log_for_task = log.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let log = log_for_task.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let log = log.clone();
                    async move {
                        let method = req.method().clone();
                        let path = req.uri().path().to_string();
                        let mut headers = BTreeMap::new();
                        for (k, v) in req.headers() {
                            if let Ok(s) = v.to_str() {
                                headers.insert(k.as_str().to_string(), s.to_string());
                            }
                        }
                        let body_bytes = req.collect().await.unwrap().to_bytes();
                        let body_text = String::from_utf8_lossy(&body_bytes).into_owned();
                        log.lock().unwrap().push(Captured {
                            method: method.as_str().to_string(),
                            path: path.clone(),
                            body: body_text.clone(),
                            headers,
                        });

                        let resp: HRes<Full<Bytes>> = if path == "/json" {
                            HRes::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .body(Full::from(r#"{"hello":"world"}"#))
                                .unwrap()
                        } else if path == "/500" {
                            HRes::builder()
                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                .body(Full::from("boom"))
                                .unwrap()
                        } else if method == Method::POST {
                            HRes::builder()
                                .status(StatusCode::OK)
                                .body(Full::from(format!("posted: {body_text}")))
                                .unwrap()
                        } else {
                            HRes::builder()
                                .status(StatusCode::OK)
                                .body(Full::from(format!("got {path}")))
                                .unwrap()
                        };
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    (addr, log)
}

#[tokio::test]
async fn get_returns_body_and_status() {
    let (addr, _log) = spawn_echo().await;
    let res = send(Request {
        method: "GET".into(),
        url: format!("http://{addr}/hello"),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "got /hello");
}

#[tokio::test]
async fn post_sends_body() {
    let (addr, log) = spawn_echo().await;
    let res = send(Request {
        method: "POST".into(),
        url: format!("http://{addr}/echo"),
        body: Some("ping".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "posted: ping");
    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].method, "POST");
    assert_eq!(captured[0].body, "ping");
}

#[tokio::test]
async fn post_json_sets_content_type() {
    let (addr, log) = spawn_echo().await;
    let res = send(Request {
        method: "POST".into(),
        url: format!("http://{addr}/echo"),
        json: Some(serde_json::json!({"k":"v"})),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(res.status, 200);
    let captured = log.lock().unwrap();
    assert_eq!(
        captured[0].headers.get("content-type").map(|s| s.as_str()),
        Some("application/json")
    );
    assert!(captured[0].body.contains("\"k\":\"v\""));
}

#[tokio::test]
async fn forwards_custom_headers() {
    let (addr, log) = spawn_echo().await;
    let mut headers = BTreeMap::new();
    headers.insert("x-agentd-test".into(), "abc".into());
    send(Request {
        method: "GET".into(),
        url: format!("http://{addr}/h"),
        headers,
        ..Default::default()
    })
    .await
    .unwrap();
    let captured = log.lock().unwrap();
    assert_eq!(
        captured[0].headers.get("x-agentd-test").map(|s| s.as_str()),
        Some("abc")
    );
}

#[tokio::test]
async fn passes_through_5xx_without_error() {
    let (addr, _) = spawn_echo().await;
    let res = send(Request {
        url: format!("http://{addr}/500"),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(res.status, 500);
    assert_eq!(res.body, "boom");
}

#[tokio::test]
async fn bad_method_is_error() {
    let err = send(Request {
        method: "BREW".into(),
        url: "http://localhost/".into(),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(err, agentd_http::HttpError::BadMethod(_)));
}

#[tokio::test]
async fn invalid_url_is_error() {
    let err = send(Request {
        url: "not a url".into(),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        agentd_http::HttpError::Transport(_)
            | agentd_http::HttpError::Build(_)
            | agentd_http::HttpError::InvalidUrl(_, _)
    ));
}

#[tokio::test]
async fn connection_refused_is_transport_error() {
    // Random unused port — connection refused.
    let err = send(Request {
        url: "http://127.0.0.1:1/".into(),
        timeout_ms: Some(500),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(err, agentd_http::HttpError::Transport(_)));
}

// HTTP client primitive.
//
// No permission enforcement here. Scripting/context layer gates by
// `net:<host>` before calling `send`. We surface a clean Request/Response
// shape so Lua doesn't have to think about builders.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Uppercase verb. Default `GET`.
    #[serde(default = "default_method")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// String body (use base64 elsewhere if you need binary). Mutually exclusive with `json`.
    #[serde(default)]
    pub body: Option<String>,
    /// JSON body; serialized + content-type set automatically.
    #[serde(default)]
    pub json: Option<serde_json::Value>,
    /// Optional request timeout in milliseconds. Default 30s.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

fn default_method() -> String {
    "GET".into()
}

impl Default for Request {
    fn default() -> Self {
        Self {
            method: default_method(),
            url: String::new(),
            headers: BTreeMap::new(),
            body: None,
            json: None,
            timeout_ms: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

#[derive(Debug, Error)]
pub enum HttpError {
    #[error("the URL `{0}` is not valid ({1})")]
    InvalidUrl(String, String),
    #[error("the HTTP method `{0}` is not supported")]
    BadMethod(String),
    #[error("could not build the HTTP request ({0})")]
    Build(String),
    #[error("the HTTP request could not reach the server ({0})")]
    Transport(String),
    #[error("could not decode the response body ({0})")]
    Decode(String),
    #[error("the server redirected the request to `{url}`, which is not allowed ({reason})")]
    RedirectRefused { url: String, reason: String },
    #[error("`{host}` resolves only to addresses that are not allowed ({reason})")]
    AddressRefused { host: String, reason: String },
}

/// Decides whether the request may connect to one resolved address. Returns
/// why it is refused, if it is.
pub type AddressCheck = Arc<dyn Fn(std::net::IpAddr) -> Result<(), String> + Send + Sync>;

/// What a checked request must pass besides its first URL: every redirect
/// target, and every address a host name resolves to.
pub struct Checks {
    pub redirect: RedirectCheck,
    pub address: AddressCheck,
}

/// Resolves host names with the system resolver and drops every address the
/// check refuses, so the client can never connect to one. When nothing is
/// left, the refusal is recorded for [`send_checked`] to report.
struct FilteringResolver {
    check: AddressCheck,
    refused: Arc<Mutex<Option<(String, String)>>>,
}

impl reqwest::dns::Resolve for FilteringResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let check = self.check.clone();
        let refused = self.refused.clone();
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
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
            if allowed.is_empty()
                && let Some(reason) = last_reason
            {
                *refused.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some((host.clone(), reason.clone()));
                return Err(
                    format!("`{host}` resolves only to refused addresses ({reason})").into(),
                );
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Decides whether a redirect may be followed. Receives the target URL and
/// returns why it is refused, if it is.
pub type RedirectCheck = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Most redirects followed for one request, matching the client's default.
const MAX_REDIRECTS: usize = 10;

/// Parse the host out of a URL. Used to derive the `net:<host>` permission slug.
pub fn host_of(url: &str) -> Result<String, HttpError> {
    let parsed =
        url::Url::parse(url).map_err(|e| HttpError::InvalidUrl(url.to_string(), e.to_string()))?;
    parsed
        .host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| HttpError::InvalidUrl(url.to_string(), "no host".into()))
}

/// Like [`send`], but hands each body chunk to `on_chunk` as it arrives —
/// the primitive under SSE-style provider streaming. `on_chunk` fires only
/// for 2xx responses (an error body carries no events worth relaying); the
/// full body is accumulated and returned either way, so callers keep the
/// complete response and the existing status/error handling.
pub async fn send_streaming(
    req: Request,
    mut on_chunk: impl FnMut(&[u8]) + Send,
) -> Result<Response, HttpError> {
    let mut resp = begin(req).await?;
    let status = resp.status().as_u16();
    let stream_chunks = (200..300).contains(&status);
    let mut headers = BTreeMap::new();
    for (k, v) in resp.headers() {
        if let Ok(s) = v.to_str() {
            headers.insert(k.as_str().to_string(), s.to_string());
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| HttpError::Transport(e.to_string()))?
    {
        if stream_chunks {
            on_chunk(&chunk);
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body).map_err(|e| HttpError::Decode(e.to_string()))?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

/// Build and dispatch the request, returning the live `reqwest::Response`.
async fn begin(req: Request) -> Result<reqwest::Response, HttpError> {
    begin_with(req, reqwest::redirect::Policy::default(), None).await
}

async fn begin_with(
    req: Request,
    redirects: reqwest::redirect::Policy,
    resolver: Option<FilteringResolver>,
) -> Result<reqwest::Response, HttpError> {
    let method = req.method.to_uppercase();
    let method = match method.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "PATCH" => reqwest::Method::PATCH,
        "DELETE" => reqwest::Method::DELETE,
        "HEAD" => reqwest::Method::HEAD,
        "OPTIONS" => reqwest::Method::OPTIONS,
        other => return Err(HttpError::BadMethod(other.to_string())),
    };

    let timeout = Duration::from_millis(req.timeout_ms.unwrap_or(30_000));
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(redirects);
    if let Some(resolver) = resolver {
        // A proxy would resolve the host itself, out of reach of the check.
        builder = builder.no_proxy().dns_resolver(Arc::new(resolver));
    }
    let client = builder
        .build()
        .map_err(|e| HttpError::Build(e.to_string()))?;

    let mut rb = client.request(method, &req.url);
    for (k, v) in &req.headers {
        rb = rb.header(k, v);
    }
    rb = match (&req.json, &req.body) {
        (Some(v), _) => rb.json(v),
        (None, Some(b)) => rb.body(b.clone()),
        _ => rb,
    };

    rb.send()
        .await
        .map_err(|e| HttpError::Transport(e.to_string()))
}

pub async fn send(req: Request) -> Result<Response, HttpError> {
    read_response(begin(req).await?).await
}

/// Like [`send`], but every redirect target must pass `checks.redirect`
/// before it is requested, and every address a host name resolves to must
/// pass `checks.address` before it is connected to. A refused hop ends the
/// request with [`HttpError::RedirectRefused`] and a host with only refused
/// addresses with [`HttpError::AddressRefused`]; neither is ever contacted.
/// Environment proxies are not used. An IP literal in a URL is not resolved,
/// so the caller checks it with the URL itself.
pub async fn send_checked(req: Request, checks: Checks) -> Result<Response, HttpError> {
    let Checks {
        redirect: check,
        address,
    } = checks;
    let refused: Arc<Mutex<Option<(String, String)>>> = Arc::default();
    let slot = refused.clone();
    let refused_address: Arc<Mutex<Option<(String, String)>>> = Arc::default();
    let resolver = FilteringResolver {
        check: address,
        refused: refused_address.clone(),
    };
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        let url = attempt.url().to_string();
        match check(&url) {
            Ok(()) => attempt.follow(),
            Err(reason) => {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some((url, reason));
                attempt.stop()
            }
        }
    });
    let resp = match begin_with(req, policy, Some(resolver)).await {
        Ok(resp) => resp,
        Err(e) => {
            if let Some((host, reason)) = refused_address
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                return Err(HttpError::AddressRefused { host, reason });
            }
            return Err(e);
        }
    };
    if let Some((url, reason)) = refused.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(HttpError::RedirectRefused { url, reason });
    }
    read_response(resp).await
}

async fn read_response(resp: reqwest::Response) -> Result<Response, HttpError> {
    let status = resp.status().as_u16();
    let mut headers = BTreeMap::new();
    for (k, v) in resp.headers() {
        if let Ok(s) = v.to_str() {
            headers.insert(k.as_str().to_string(), s.to_string());
        }
    }
    let body = resp
        .text()
        .await
        .map_err(|e| HttpError::Decode(e.to_string()))?;
    Ok(Response {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_extraction() {
        assert_eq!(
            host_of("https://api.github.com/x").unwrap(),
            "api.github.com"
        );
        assert_eq!(host_of("http://localhost:8080/").unwrap(), "localhost");
    }

    #[test]
    fn host_rejects_bad_url() {
        assert!(host_of("not a url").is_err());
        assert!(host_of("file:///tmp/x").is_err()); // no host
    }
}

#[cfg(test)]
mod integration_tests {
    //! Live HTTP integration tests against a tiny local hyper server.

    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use super::{Request, send};
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
        assert!(matches!(err, super::HttpError::BadMethod(_)));
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
            super::HttpError::Transport(_)
                | super::HttpError::Build(_)
                | super::HttpError::InvalidUrl(_, _)
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
        assert!(matches!(err, super::HttpError::Transport(_)));
    }

    fn checks(address: super::AddressCheck) -> super::Checks {
        super::Checks {
            redirect: Arc::new(|_| Ok(())),
            address,
        }
    }

    #[tokio::test]
    async fn a_name_resolving_only_to_refused_addresses_is_never_contacted() {
        let (addr, log) = spawn_echo().await;
        let refuse_loopback: super::AddressCheck = Arc::new(|ip: std::net::IpAddr| {
            if ip.is_loopback() {
                Err(format!("`net:{ip}` is denied"))
            } else {
                Ok(())
            }
        });
        let err = super::send_checked(
            Request {
                url: format!("http://localhost:{}/x", addr.port()),
                ..Default::default()
            },
            checks(refuse_loopback),
        )
        .await
        .unwrap_err();
        match err {
            super::HttpError::AddressRefused { host, reason } => {
                assert_eq!(host, "localhost");
                assert!(reason.contains("denied"), "{reason}");
            }
            other => panic!("expected AddressRefused, got {other:?}"),
        }
        assert!(
            log.lock().unwrap().is_empty(),
            "the server was never contacted"
        );
    }

    #[tokio::test]
    async fn allowed_addresses_still_connect() {
        let (addr, _log) = spawn_echo().await;
        let res = super::send_checked(
            Request {
                url: format!("http://localhost:{}/x", addr.port()),
                ..Default::default()
            },
            checks(Arc::new(|_| Ok(()))),
        )
        .await
        .unwrap();
        assert_eq!(res.body, "got /x");
    }
}

//! HTTP client primitive.
//!
//! No permission enforcement here. Scripting/context layer gates by
//! `net:<host>` before calling `send`. We surface a clean Request/Response
//! shape so Lua doesn't have to think about builders.

use std::collections::BTreeMap;
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
    #[error("invalid url `{0}`: {1}")]
    InvalidUrl(String, String),
    #[error("unsupported method `{0}`")]
    BadMethod(String),
    #[error("request build: {0}")]
    Build(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("decode body: {0}")]
    Decode(String),
}

/// Parse the host out of a URL. Used to derive the `net:<host>` permission slug.
pub fn host_of(url: &str) -> Result<String, HttpError> {
    let parsed =
        url::Url::parse(url).map_err(|e| HttpError::InvalidUrl(url.to_string(), e.to_string()))?;
    parsed
        .host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| HttpError::InvalidUrl(url.to_string(), "no host".into()))
}

pub async fn send(req: Request) -> Result<Response, HttpError> {
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
    let client = reqwest::Client::builder()
        .timeout(timeout)
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

    let resp = rb
        .send()
        .await
        .map_err(|e| HttpError::Transport(e.to_string()))?;
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

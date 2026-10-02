//! `ctx.http` redirects: every hop must pass the same `net:` check as the
//! first URL, or the request is refused before the hop is fetched.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

fn ctx(grants: &[&str]) -> CallContext {
    CallContext {
        caller: Caller::interface("test"),
        effective_grants: PermissionSet::from_iter(grants.iter().copied()),
        call_chain: Vec::new(),
        cwd: None,
    }
}

/// Serves `/same` (redirect to `/landed` on the same host), `/away`
/// (redirect to the same server under the host name `localhost`), and
/// `/landed`. Records every path it was asked for.
async fn spawn_redirect_server() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let log = log.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let log = log.clone();
                    async move {
                        let path = req.uri().path().to_string();
                        log.lock().unwrap().push(path.clone());
                        let builder = hyper::Response::builder();
                        let resp: hyper::Response<Full<Bytes>> = match path.as_str() {
                            "/same" => builder.status(302).header("location", "/landed"),
                            "/away" => builder.status(302).header(
                                "location",
                                format!("http://localhost:{}/landed", addr.port()),
                            ),
                            _ => builder.status(200),
                        }
                        .body(Full::from(
                            if path == "/landed" { "landed" } else { "" }.to_string(),
                        ))
                        .unwrap();
                        Ok::<_, Infallible>(resp)
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, seen)
}

fn host_with_get(addr: SocketAddr, path: &str) -> (tempfile::TempDir, LuaHost) {
    let dir = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(dir.path().join("t.lua")).unwrap();
    write!(
        f,
        r#"
        agentd.action{{
          name = "h.go",
          handler = function(_, ctx)
            local r = ctx.http.get("http://{addr}{path}")
            return {{ status = r.status, body = r.body }}
          end,
        }}
        "#
    )
    .unwrap();
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    (dir, host)
}

fn go() -> ActionCall {
    ActionCall {
        action: "h.go".into(),
        args: serde_json::Value::Null,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_within_a_granted_host_is_followed() {
    let (addr, _) = spawn_redirect_server().await;
    let (_dir, host) = host_with_get(addr, "/same");
    let res = host.call(ctx(&["net:127.0.0.1"]), go()).await.unwrap();
    assert_eq!(res.value["status"], 200);
    assert_eq!(res.value["body"], "landed");
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_to_an_ungranted_host_is_refused_before_it_is_fetched() {
    let (addr, seen) = spawn_redirect_server().await;
    let (_dir, host) = host_with_get(addr, "/away");
    let err = host
        .call(ctx(&["net:127.0.0.1"]), go())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("net:localhost"),
        "names the missing grant: {err}"
    );
    assert!(err.contains("redirect"), "says it was a redirect: {err}");
    assert_eq!(
        *seen.lock().unwrap(),
        ["/away"],
        "the redirect target was never requested"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_to_another_granted_host_is_followed() {
    let (addr, _) = spawn_redirect_server().await;
    let (_dir, host) = host_with_get(addr, "/away");
    let res = host
        .call(ctx(&["net:127.0.0.1", "net:localhost"]), go())
        .await
        .unwrap();
    assert_eq!(res.value["body"], "landed");
}

/// Denies `net:localhost`, the host `/away` redirects to.
struct DenyLocalhost;
impl agentd_types::Denials for DenyLocalhost {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter(["net:localhost"])
    }
    fn denies_action(&self, _name: &str) -> bool {
        false
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn redirect_to_a_denied_host_is_refused_even_under_a_wildcard_grant() {
    let (addr, seen) = spawn_redirect_server().await;
    let (_dir, host) = host_with_get(addr, "/away");
    host.set_denials(Arc::new(DenyLocalhost));
    let err = host
        .call(ctx(&["net:*"]), go())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("net:localhost"), "{err}");
    assert!(err.contains("denied"), "{err}");
    assert_eq!(
        *seen.lock().unwrap(),
        ["/away"],
        "the denied host was never requested"
    );
}

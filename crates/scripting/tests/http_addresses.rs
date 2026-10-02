//! A host name that resolves only to addresses the operator denied is never
//! contacted by `ctx.http`, even when the name itself is granted.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Denials, Registry};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

/// Denies both loopback addresses, which is everything `localhost` resolves to.
struct DenyLoopback;
impl Denials for DenyLoopback {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter(["net:127.0.0.1", "net:::1"])
    }
    fn denies_action(&self, _name: &str) -> bool {
        false
    }
}

async fn spawn_server() -> (SocketAddr, Arc<Mutex<usize>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(Mutex::new(0));
    let count = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let count = count.clone();
            tokio::spawn(async move {
                let service = service_fn(move |_req: hyper::Request<hyper::body::Incoming>| {
                    *count.lock().unwrap() += 1;
                    async {
                        Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from("ok"))))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (addr, hits)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_granted_name_resolving_to_denied_addresses_is_refused() {
    let (addr, hits) = spawn_server().await;
    let dir = tempfile::tempdir().unwrap();
    write!(
        std::fs::File::create(dir.path().join("t.lua")).unwrap(),
        r#"
        agentd.action{{
          name = "h.go",
          handler = function(_, ctx)
            return {{ body = ctx.http.get("http://localhost:{}/").body }}
          end,
        }}
        "#,
        addr.port()
    )
    .unwrap();
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    host.set_denials(Arc::new(DenyLoopback));
    let err = host
        .call(
            CallContext {
                caller: Caller::interface("test"),
                effective_grants: PermissionSet::from_iter(["net:localhost"]),
                call_chain: Vec::new(),
                cwd: None,
            },
            ActionCall {
                action: "h.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("localhost"), "{err}");
    assert!(err.contains("denied"), "{err}");
    assert_eq!(*hits.lock().unwrap(), 0, "the server was never contacted");
}

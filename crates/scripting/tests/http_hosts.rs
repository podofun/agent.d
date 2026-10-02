//! A `net:` grant matches its host however either side is written: case,
//! a trailing dot, or both.

use std::convert::Infallible;
use std::io::Write;
use std::net::SocketAddr;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;

async fn spawn_ok_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let service = service_fn(|_req: hyper::Request<hyper::body::Incoming>| async {
                    Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from("ok"))))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn grant_and_url_match_across_case_and_trailing_dot() {
    let addr = spawn_ok_server().await;
    let dir = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(dir.path().join("t.lua")).unwrap();
    write!(
        f,
        r#"
        agentd.action{{
          name = "h.go",
          handler = function(_, ctx)
            return {{ body = ctx.http.get("http://localhost.:{}/").body }}
          end,
        }}
        "#,
        addr.port()
    )
    .unwrap();
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            CallContext {
                caller: Caller::interface("test"),
                effective_grants: PermissionSet::from_iter(["net:LocalHost"]),
                call_chain: Vec::new(),
                cwd: None,
            },
            ActionCall {
                action: "h.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value["body"], "ok");
}

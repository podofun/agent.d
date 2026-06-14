//! context.fs + context.http (P2) integration tests.

use std::io::Write;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};

fn write_tools(scripts: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in scripts {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }
    dir
}

fn ctx(grants: &[&str]) -> CallContext {
    CallContext {
        caller: Caller::interface("test"),
        effective_grants: PermissionSet::from_iter(grants.iter().copied()),
        call_chain: Vec::new(),
    }
}

// ---------- fs ----------

#[tokio::test(flavor = "multi_thread")]
async fn fs_read_denied_without_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("hello.txt");
    std::fs::write(&target, "hi").unwrap();
    let target_str = target.to_string_lossy().into_owned();

    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "r.read",
              handler = function(_, ctx)
                return {{ s = ctx.fs.read("{target_str}") }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[]),
            ActionCall {
                action: "r.read".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("fs.read"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_read_allowed_with_scoped_grant() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("hello.txt");
    std::fs::write(&target, "hello world").unwrap();
    let target_str = target.to_string_lossy().into_owned();
    let glob = format!("fs.read:{}/**", tmp.path().display());

    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "r.read",
              handler = function(_, ctx)
                return {{ s = ctx.fs.read("{target_str}") }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&[&glob]),
            ActionCall {
                action: "r.read".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let s = res.value.get("s").and_then(|v| v.as_str()).unwrap();
    assert_eq!(s, "hello world");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_write_and_read_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("out.txt");
    let target_str = target.to_string_lossy().into_owned();
    let read_glob = format!("fs.read:{}/**", tmp.path().display());
    let write_glob = format!("fs.write:{}/**", tmp.path().display());

    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "rt.go",
              handler = function(_, ctx)
                ctx.fs.write("{target_str}", "payload")
                return {{ s = ctx.fs.read("{target_str}") }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&[&read_glob, &write_glob]),
            ActionCall {
                action: "rt.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("s").and_then(|v| v.as_str()), Some("payload"));
    let on_disk = std::fs::read_to_string(&target).unwrap();
    assert_eq!(on_disk, "payload");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_write_denied_when_only_read_granted() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("out.txt");
    let target_str = target.to_string_lossy().into_owned();
    let read_glob = format!("fs.read:{}/**", tmp.path().display());

    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "w.try",
              handler = function(_, ctx)
                ctx.fs.write("{target_str}", "x")
                return {{}}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[&read_glob]),
            ActionCall {
                action: "w.try".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("fs.write"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_list_dir_returns_entries() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"").unwrap();
    let dir_str = tmp.path().to_string_lossy().into_owned();
    let glob = format!("fs.read:{}/**", tmp.path().display());

    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "l.go",
              handler = function(_, ctx)
                local items = ctx.fs.list_dir("{dir_str}")
                local names = {{}}
                for i, e in ipairs(items) do names[i] = e.name end
                return {{ names = names }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&[&glob]),
            ActionCall {
                action: "l.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let names = res.value.get("names").and_then(|v| v.as_array()).unwrap();
    let names: Vec<&str> = names.iter().filter_map(|v| v.as_str()).collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
}

// ---------- http ----------

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;

async fn spawn_ok_server() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (s, _) = match listener.accept().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            tokio::spawn(async move {
                let io = TokioIo::new(s);
                let service =
                    service_fn(|_req: hyper::Request<hyper::body::Incoming>| async move {
                        let r: hyper::Response<Full<Bytes>> = hyper::Response::builder()
                            .status(200)
                            .body(Full::from("ok"))
                            .unwrap();
                        Ok::<_, Infallible>(r)
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, service)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn http_get_denied_without_net_grant() {
    let addr = spawn_ok_server().await;
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "h.go",
              handler = function(_, ctx)
                return ctx.http.get("http://{addr}/p")
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[]),
            ActionCall {
                action: "h.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("net:"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn http_get_allowed_with_host_grant() {
    let addr = spawn_ok_server().await;
    let grant = format!("net:{}", addr.ip());
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "h.go",
              handler = function(_, ctx)
                local r = ctx.http.get("http://{addr}/p")
                return {{ status = r.status, body = r.body }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&[&grant]),
            ActionCall {
                action: "h.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("status").and_then(|v| v.as_i64()), Some(200));
    assert_eq!(res.value.get("body").and_then(|v| v.as_str()), Some("ok"));
}

#[tokio::test(flavor = "multi_thread")]
async fn http_wildcard_grant_covers_any_host() {
    let addr = spawn_ok_server().await;
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "h.go",
              handler = function(_, ctx)
                local r = ctx.http.get("http://{addr}/p")
                return {{ status = r.status }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["net:*"]),
            ActionCall {
                action: "h.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("status").and_then(|v| v.as_i64()), Some(200));
}

// ---------- fs path-scope bypass guards ----------
//
// A path-scoped grant (`fs.read:<dir>/**`) must hold against the two classic
// escapes: a symlink whose target lives outside the grant, and a `..` segment
// that climbs out of it. Both rely on the binding resolving the path BEFORE
// deriving the permission slug — otherwise the slug describes the alias, not
// the file actually touched.

#[tokio::test(flavor = "multi_thread")]
async fn fs_read_symlink_escape_denied() {
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "TOPSECRET").unwrap();
    // A symlink inside the granted dir pointing at the out-of-scope secret.
    let link = granted.path().join("link.txt");
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    let link_str = link.to_string_lossy().into_owned();
    let glob = format!("fs.read:{}/**", granted.path().display());

    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "r.link",
              handler = function(_, ctx)
                return {{ s = ctx.fs.read("{link_str}") }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[&glob]),
            ActionCall {
                action: "r.link".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("fs.read"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn fs_read_parent_traversal_denied() {
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, "S").unwrap();
    // Grant only the sub dir; climb out of it with `..`.
    let glob = format!("fs.read:{}/**", sub.display());
    let escape = format!("{}/../secret.txt", sub.display());

    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "r.up",
              handler = function(_, ctx)
                return {{ s = ctx.fs.read("{escape}") }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[&glob]),
            ActionCall {
                action: "r.up".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("fs.read"), "got {err}");
}

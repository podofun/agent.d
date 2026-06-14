//! context.ws integration tests against a local tungstenite echo server.

use std::io::Write;
use std::net::SocketAddr;

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
                    use tokio_tungstenite::tungstenite::Message;
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

#[tokio::test(flavor = "multi_thread")]
async fn ws_text_roundtrip_with_grant() {
    let addr = spawn_echo().await;
    let url = format!("ws://{addr}/");
    let grant = format!("net:{}", addr.ip());
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "ws.echo",
              handler = function(_, ctx)
                local h = ctx.ws.connect("{url}")
                h:send("hello")
                local f = h:recv(2000)
                h:close()
                return {{ kind = f.kind, text = f.text }}
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
                action: "ws.echo".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("kind").and_then(|v| v.as_str()), Some("text"));
    assert_eq!(
        res.value.get("text").and_then(|v| v.as_str()),
        Some("hello")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_connect_denied_without_grant() {
    let addr = spawn_echo().await;
    let url = format!("ws://{addr}/");
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "ws.no",
              handler = function(_, ctx)
                local h = ctx.ws.connect("{url}")
                return {{ ok = true }}
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
                action: "ws.no".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("net:"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_recv_times_out() {
    let addr = spawn_echo().await;
    let url = format!("ws://{addr}/");
    let grant = format!("net:{}", addr.ip());
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "ws.timeout",
              handler = function(_, ctx)
                local h = ctx.ws.connect("{url}")
                -- don't send; server is silent
                local ok, err = pcall(function() return h:recv(100) end)
                h:close()
                return {{ ok = ok, err = tostring(err) }}
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
                action: "ws.timeout".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("ok").and_then(|v| v.as_bool()), Some(false));
    let err = res.value.get("err").and_then(|v| v.as_str()).unwrap();
    assert!(err.contains("timeout"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_send_binary_roundtrip() {
    let addr = spawn_echo().await;
    let url = format!("ws://{addr}/");
    let grant = format!("net:{}", addr.ip());
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "ws.bin",
              handler = function(_, ctx)
                local h = ctx.ws.connect("{url}")
                h:send_binary("\x01\x02\x03")
                local f = h:recv(2000)
                h:close()
                return {{ kind = f.kind, len = #f.binary }}
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
                action: "ws.bin".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        res.value.get("kind").and_then(|v| v.as_str()),
        Some("binary")
    );
    assert_eq!(res.value.get("len").and_then(|v| v.as_i64()), Some(3));
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_handle_is_closed_after_close() {
    let addr = spawn_echo().await;
    let url = format!("ws://{addr}/");
    let grant = format!("net:{}", addr.ip());
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "ws.close",
              handler = function(_, ctx)
                local h = ctx.ws.connect("{url}")
                h:close()
                return {{ closed = h:is_closed() }}
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
                action: "ws.close".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        res.value.get("closed").and_then(|v| v.as_bool()),
        Some(true)
    );
}

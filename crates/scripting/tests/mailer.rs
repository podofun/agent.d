//! `ctx.mailer` integration tests against a minimal local SMTP mock.
//!
//! Mirrors `context_p4_ws.rs` for host/grant/runtime mechanics and copies a
//! local plaintext SMTP mock (same minimal state machine as the net-crate
//! mailer tests) so we don't reach across crates for a private test helper.

use std::io::Write;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

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
        cwd: None,
    }
}

/// Captured from one SMTP session.
#[derive(Debug, Default, Clone)]
struct Captured {
    rcpts: Vec<String>,
    data: String,
}

/// Minimal plaintext SMTP state machine — enough for lettre's client to send a
/// single message and for us to capture RCPTs + DATA. Breaks out of the read
/// loop right after capturing DATA so lettre's pooled connection doesn't hang.
async fn serve_session(stream: tokio::net::TcpStream) -> Captured {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut captured = Captured::default();

    write_half.write_all(b"220 mock ESMTP\r\n").await.unwrap();

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await.unwrap();
        if n == 0 {
            break;
        }
        let upper = line.to_uppercase();
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            write_half
                .write_all(b"250-mock\r\n250 OK\r\n")
                .await
                .unwrap();
        } else if upper.starts_with("MAIL FROM") {
            write_half.write_all(b"250 OK\r\n").await.unwrap();
        } else if upper.starts_with("RCPT TO") {
            captured.rcpts.push(line.trim().to_string());
            write_half.write_all(b"250 OK\r\n").await.unwrap();
        } else if upper.starts_with("DATA") {
            write_half
                .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                .await
                .unwrap();
            let mut body = String::new();
            loop {
                let mut dl = String::new();
                let dn = reader.read_line(&mut dl).await.unwrap();
                if dn == 0 {
                    break;
                }
                if dl == ".\r\n" || dl == ".\n" {
                    break;
                }
                body.push_str(&dl);
            }
            captured.data = body;
            write_half
                .write_all(b"250 Ok: queued as MOCK123\r\n")
                .await
                .unwrap();
        } else if upper.starts_with("QUIT") {
            write_half.write_all(b"221 Bye\r\n").await.unwrap();
            break;
        } else {
            write_half.write_all(b"250 OK\r\n").await.unwrap();
        }
        // Once a full message is captured we have everything; returning here
        // avoids blocking on lettre's pool keep-alive.
        if !captured.data.is_empty() {
            break;
        }
    }
    captured
}

/// Bind a one-shot mock SMTP server; returns the bound port and a receiver that
/// yields the captured session.
async fn mock_smtp() -> (u16, oneshot::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let captured = serve_session(stream).await;
        let _ = tx.send(captured);
    });
    (port, rx)
}

/// Mock that accepts N sequential sessions, returning all captures.
async fn mock_smtp_n(n: usize) -> (u16, oneshot::Receiver<Vec<Captured>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let mut all = Vec::new();
        for _ in 0..n {
            let (stream, _) = listener.accept().await.unwrap();
            all.push(serve_session(stream).await);
        }
        let _ = tx.send(all);
    });
    (port, rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn denied_without_grant() {
    // No async runtime needed: `create` gates on `net:<host>` before connecting.
    let port = 2525;
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "mail.no",
              handler = function(_, ctx)
                local m = ctx.mailer.create{{
                  host = "127.0.0.1", port = {port},
                  from = "a@b.c", security = "plaintext",
                }}
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
                action: "mail.no".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("net:127.0.0.1"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn send_end_to_end() {
    let (port, rx) = mock_smtp().await;
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "mail.send",
              handler = function(_, ctx)
                local m = ctx.mailer.create{{
                  host = "127.0.0.1", port = {port},
                  from = "a@b.c", security = "plaintext",
                }}
                -- run through a coroutine so the yieldable send path is taken
                local h = async(function()
                  return m:send{{ to = {{ "x@y.z" }}, subject = "hello-e2e", text = "body-e2e" }}
                end)
                local r = await(h)
                return {{ ok = r.ok }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["net:127.0.0.1"]),
            ActionCall {
                action: "mail.send".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("ok").and_then(|v| v.as_bool()), Some(true));

    let cap = rx.await.unwrap();
    assert!(
        cap.data.contains("Subject: hello-e2e"),
        "data: {}",
        cap.data
    );
    assert!(
        cap.rcpts.iter().any(|r| r.contains("x@y.z")),
        "rcpts: {:?}",
        cap.rcpts
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_subject_errors() {
    let (port, _rx) = mock_smtp().await;
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "mail.nosubj",
              handler = function(_, ctx)
                local m = ctx.mailer.create{{
                  host = "127.0.0.1", port = {port},
                  from = "a@b.c", security = "plaintext",
                }}
                local ok, err = pcall(function()
                  return m:send{{ to = {{ "x@y.z" }}, text = "body" }}
                end)
                return {{ ok = ok, err = tostring(err) }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["net:127.0.0.1"]),
            ActionCall {
                action: "mail.nosubj".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("ok").and_then(|v| v.as_bool()), Some(false));
    let err = res.value.get("err").and_then(|v| v.as_str()).unwrap();
    assert!(err.contains("`subject` is required"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_sends() {
    // Two concurrent `:send`s through the scheduler — proves send yields rather
    // than blocking the scheduler. The mock accepts two sequential sessions.
    let (port, rx) = mock_smtp_n(2).await;
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{
              name = "mail.par",
              handler = function(_, ctx)
                local m = ctx.mailer.create{{
                  host = "127.0.0.1", port = {port},
                  from = "a@b.c", security = "plaintext",
                }}
                local a = async(function()
                  return m:send{{ to = {{ "a1@y.z" }}, subject = "subj-a", text = "ba" }}
                end)
                local b = async(function()
                  return m:send{{ to = {{ "b1@y.z" }}, subject = "subj-b", text = "bb" }}
                end)
                return {{ a = await(a).ok, b = await(b).ok }}
              end,
            }}
            "#
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["net:127.0.0.1"]),
            ActionCall {
                action: "mail.par".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("a").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(res.value.get("b").and_then(|v| v.as_bool()), Some(true));

    let caps = rx.await.unwrap();
    let subjects: String = caps.iter().map(|c| c.data.clone()).collect();
    assert!(subjects.contains("subj-a"), "captures: {subjects}");
    assert!(subjects.contains("subj-b"), "captures: {subjects}");
}

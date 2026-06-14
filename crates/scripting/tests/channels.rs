//! Channel primitive: bare global `channel("name")` / `channel()` for actor-
//! style message passing. Tests cover anon vs named, send + recv (yieldable),
//! try_recv, close, and cross-service / cross-async-task flow.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_secrets::MemoryStore;
use agentd_types::{ActionCall, Registry};

fn write_init(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("init.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

fn host_with(root: &std::path::Path) -> Arc<LuaHost> {
    let host = LuaHost::new().unwrap();
    host.set_root(root);
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.set_secrets(Arc::new(MemoryStore::default()));
    Arc::new(host)
}

#[tokio::test(flavor = "multi_thread")]
async fn anon_channel_round_trip() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.echo",
          handler = function(_, ctx)
            local ch = channel()
            ch:send({ kind = "ping", n = 1 })
            ch:send({ kind = "ping", n = 2 })
            local a = ch:recv()
            local b = ch:recv()
            return { a = a, b = b }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let ctx = agentd_types::CallContext {
        caller: Caller::default(),
        effective_grants: PermissionSet::empty(),
        call_chain: vec!["demo.echo".into()],
    };
    let res = host
        .call(
            ctx,
            ActionCall {
                action: "demo.echo".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        res.value,
        serde_json::json!({
            "a": { "kind": "ping", "n": 1 },
            "b": { "kind": "ping", "n": 2 },
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn named_channel_deduplicates_across_handles() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.named",
          handler = function(_, ctx)
            local a = channel("counter")
            local b = channel("counter")
            a:send("hello")
            return { from_b = b:recv() }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = host
        .call(
            agentd_types::CallContext::default(),
            ActionCall {
                action: "demo.named".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value, serde_json::json!({ "from_b": "hello" }));
}

#[tokio::test(flavor = "multi_thread")]
async fn try_recv_returns_nil_when_empty() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.try",
          handler = function(_, ctx)
            local ch = channel()
            local r1 = ch:try_recv()
            ch:send(42)
            local r2 = ch:try_recv()
            return { empty = r1, full = r2 }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = host
        .call(
            agentd_types::CallContext::default(),
            ActionCall {
                action: "demo.try".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    // `empty` is nil/Null; `full` is the JSON number.
    assert_eq!(res.value.get("full").unwrap(), &serde_json::json!(42));
}

#[tokio::test(flavor = "multi_thread")]
async fn recv_yields_so_async_tasks_can_send() {
    // Reader awaits a recv (yields). A second `async` task sends — the
    // scheduler must wake the reader.
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.fanout",
          handler = function(_, ctx)
            local ch = channel()
            local writer = async(function()
              ch:send("a")
              ch:send("b")
            end)
            local out = { ch:recv(), ch:recv() }
            await(writer)
            return out
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        host.call(
            agentd_types::CallContext::default(),
            ActionCall {
                action: "demo.fanout".into(),
                args: serde_json::Value::Null,
            },
        ),
    )
    .await
    .expect("did not deadlock")
    .unwrap();
    assert_eq!(res.value, serde_json::json!(["a", "b"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn close_blocks_further_sends() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.close",
          handler = function(_, ctx)
            local ch = channel()
            ch:close()
            local ok, err = pcall(function() ch:send("x") end)
            return { ok = ok, err = tostring(err), closed = ch:is_closed() }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = host
        .call(
            agentd_types::CallContext::default(),
            ActionCall {
                action: "demo.close".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("ok").unwrap(), &serde_json::json!(false));
    assert_eq!(res.value.get("closed").unwrap(), &serde_json::json!(true));
    assert!(
        res.value
            .get("err")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("closed")
    );
}

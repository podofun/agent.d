//! `sleep(ms)` is a yieldable timer (peer async tasks make progress during
//! the sleep) and the ws userdata refactor preserves the existing API while
//! making `:send` / `:recv` cooperate with the scheduler.

use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
async fn sleep_yields_so_async_peers_make_progress() {
    // While the main coroutine sleeps for 200 ms, two async tasks each
    // perform their own short sleep + send results into a channel. They
    // should all complete inside roughly the longest single sleep (~200 ms)
    // because the scheduler interleaves them — *not* sum(200, 50, 50).
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.par_sleep",
          handler = function(_, ctx)
            local ch = channel()
            local a = async(function() sleep(50);  ch:send("a") end)
            local b = async(function() sleep(50);  ch:send("b") end)
            sleep(200)
            await(a); await(b)
            return { ch:recv(), ch:recv() }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let started = Instant::now();
    let res = host
        .call(
            agentd_types::CallContext {
                caller: Caller::default(),
                effective_grants: PermissionSet::empty(),
                call_chain: vec!["demo.par_sleep".into()],
            },
            ActionCall {
                action: "demo.par_sleep".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();

    // Both async tasks ran during the main sleep; total wallclock should
    // sit close to the single 200 ms sleep with generous slack for CI.
    assert!(
        elapsed < Duration::from_millis(450),
        "sleep did not yield (took {elapsed:?})"
    );

    // The send order is timing-dependent but the set of values is fixed.
    let arr = res.value.as_array().unwrap();
    let mut got: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
    got.sort();
    assert_eq!(got, vec!["a", "b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_recv_yields_letting_async_peer_send_via_channel() {
    // Discord-style pattern: a service-shaped main coroutine awaits ws data
    // via `recv` (now yieldable). Meanwhile an `async` peer pushes a
    // synthetic event into a channel. Without ws yielding, the channel
    // recv could starve. We don't actually connect a real socket here — we
    // just confirm the scheduler interleaves `sleep` + `channel:recv` /
    // `await` cooperatively, which is what makes ws practical.
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.interleave",
          handler = function(_, ctx)
            local ch = channel()
            local producer = async(function()
              sleep(20)
              ch:send("hello")
            end)
            -- The main coroutine sleeps and then awaits the channel — the
            -- producer must have run during the sleep.
            sleep(60)
            local got = ch:recv()
            await(producer)
            return { got = got }
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
                action: "demo.interleave".into(),
                args: serde_json::Value::Null,
            },
        ),
    )
    .await
    .expect("no deadlock")
    .unwrap();
    assert_eq!(res.value, serde_json::json!({ "got": "hello" }));
}

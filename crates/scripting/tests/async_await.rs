//! Smoke tests for the cooperative scheduler: yieldable bindings, plain
//! `agentd.service(...)` execution, and JS-style `async(fn)` / `await(h)`.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use agentd_ai::{MockProvider, ProviderRegistry};
use agentd_permissions::{Caller, Engine, Grants, GrantsFile, ToolGrants, model::PermissionSet};
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
    host.set_ai_provider(
        "mock",
        Arc::new(MockProvider::new().with_reply("mock-reply")),
    );
    host.set_default_ai_provider("mock");
    Arc::new(host)
}

#[tokio::test(flavor = "multi_thread")]
async fn async_await_returns_value() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.async_smoke",
          handler = function(_, ctx)
            local h = async(function() return 42 end)
            return { value = await(h) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let ctx = agentd_types::CallContext {
        caller: Caller::default(),
        effective_grants: PermissionSet::empty(),
        call_chain: vec!["demo.async_smoke".into()],
    };
    let res = host
        .call(
            ctx,
            ActionCall {
                action: "demo.async_smoke".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .expect("call ok");
    assert_eq!(res.value, serde_json::json!({ "value": 42 }));
}

#[tokio::test(flavor = "multi_thread")]
async fn await_propagates_error() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo" }
        agentd.action{
          name = "demo.async_err",
          handler = function(_, ctx)
            local h = async(function() error("kaboom") end)
            local ok, err = pcall(await, h)
            return { ok = ok, err = tostring(err) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    let ctx = agentd_types::CallContext::default();
    let res = host
        .call(
            ctx,
            ActionCall {
                action: "demo.async_err".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let ok = res.value.get("ok").and_then(|v| v.as_bool()).unwrap();
    assert!(!ok, "pcall should report failure: {res:?}");
    assert!(
        res.value
            .get("err")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("kaboom")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn async_runs_concurrently_with_ai_yield() {
    // Two `async` tasks both call `ctx.ai.ask`. With the
    // scheduler their provider calls happen concurrently — total wallclock
    // < 2x single call. We use MockProvider whose .complete returns
    // immediately, so the assertion is structural (both returned the
    // expected reply), not timing-based.
    let dir = write_init(
        r#"
        agentd.tool{ name = "demo", requires = {} }
        agentd.action{
          name = "demo.par",
          handler = function(_, ctx)
            local a = async(function() return ctx.ai.ask("a").text end)
            local b = async(function() return ctx.ai.ask("b").text end)
            return { a = await(a), b = await(b) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();

    // Grant the demo tool the `ai:mock` permission so context.ai.ask passes
    // the inline check.
    let mut file = GrantsFile::default();
    file.tool.insert(
        "demo".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
        },
    );
    let engine = Engine::new(Grants::from_file(file));
    let _ = engine; // demo doesn't go through engine.check in this test

    let ctx = agentd_types::CallContext {
        caller: Caller::default(),
        effective_grants: PermissionSet::from_iter(["ai:mock"]),
        call_chain: vec!["demo.par".into()],
    };
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        host.call(
            ctx,
            ActionCall {
                action: "demo.par".into(),
                args: serde_json::Value::Null,
            },
        ),
    )
    .await
    .expect("did not deadlock")
    .expect("call ok");
    assert_eq!(
        res.value,
        serde_json::json!({ "a": "mock-reply", "b": "mock-reply" })
    );
    let _ = ProviderRegistry::new(); // touch the import so nothing is unused
}

//! ctx.memory integration tests — handle + per-operation permission gating.
//!
//! These exercise the *inline* memory gate only: `host.call` on `LuaHost`
//! checks `check_permission_inline` against `effective_grants`. The second arg
//! to `ctx(...)` is the call_chain, not an allowlist.

use std::io::Write;
use std::sync::Arc;

use agentd_memory::MemMemoryStore;
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

fn ctx(grants: &[&str], chain: &[&str]) -> CallContext {
    CallContext {
        caller: Caller::interface("test"),
        effective_grants: PermissionSet::from_iter(grants.iter().copied()),
        call_chain: chain.iter().map(|s| s.to_string()).collect(),
        cwd: None,
    }
}

fn host_with_mem() -> LuaHost {
    let host = LuaHost::new().unwrap();
    host.set_memory(Arc::new(MemMemoryStore::new()));
    host
}

#[tokio::test]
async fn memory_set_get_roundtrip() {
    let dir = write_tools(&[(
        "m.lua",
        r#"
            agentd.action{
              name = "m.rt",
              handler = function(_, ctx)
                local mem = ctx.memory.create("proj/x")
                mem:set("k", { n = 42 })
                return mem:get("k")
              end,
            }
        "#,
    )]);
    let host = host_with_mem();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["memory.read:proj/x", "memory.write:proj/x"], &["m.rt"]),
            ActionCall {
                action: "m.rt".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value, serde_json::json!({ "n": 42 }));
}

#[tokio::test]
async fn memory_write_denied_without_grant() {
    let dir = write_tools(&[(
        "m.lua",
        r#"
            agentd.action{ name = "m.w", handler = function(_, ctx)
              ctx.memory.create("proj/x"):set("k", 1)
            end }
        "#,
    )]);
    let host = host_with_mem();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&["memory.read:proj/x"], &["m.w"]), // read only, no write
            ActionCall {
                action: "m.w".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("memory.write:proj/x"),
        "got: {err}"
    );
}

#[tokio::test]
async fn memory_wildcard_grant_covers_subtree() {
    let dir = write_tools(&[(
        "m.lua",
        r#"
            agentd.action{ name = "m.k", handler = function(_, ctx)
              local mem = ctx.memory.create("discord/chan/1")
              mem:set("a", 1); mem:set("b", 2)
              return mem:keys()
            end }
        "#,
    )]);
    let host = host_with_mem();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(
                &["memory.read:discord/**", "memory.write:discord/**"],
                &["m.k"],
            ),
            ActionCall {
                action: "m.k".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value, serde_json::json!(["a", "b"]));
}

#[tokio::test]
async fn memory_create_rejects_empty_ns() {
    let dir = write_tools(&[(
        "m.lua",
        r#"
            agentd.action{ name = "m.bad", handler = function(_, ctx)
              return ctx.memory.create("")
            end }
        "#,
    )]);
    let host = host_with_mem();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[], &["m.bad"]),
            ActionCall {
                action: "m.bad".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ctx.memory.create"), "got: {err}");
}

#[tokio::test]
async fn memory_no_backend_configured() {
    let dir = write_tools(&[(
        "m.lua",
        r#"
            agentd.action{ name = "m.nb", handler = function(_, ctx)
              return ctx.memory.create("p"):get("k")
            end }
        "#,
    )]);
    let host = LuaHost::new().unwrap(); // no set_memory
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&["memory.read:p"], &["m.nb"]),
            ActionCall {
                action: "m.nb".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no memory backend"), "got: {err}");
}

#[tokio::test]
async fn memory_get_returns_default_when_absent() {
    let dir = write_tools(&[(
        "m.lua",
        r#"
            agentd.action{
              name = "m.def",
              handler = function(_, ctx)
                local mem = ctx.memory.create("proj/x")
                local missing = mem:get("nope", { fallback = true })
                mem:set("k", false)
                return {
                  missing = missing,
                  -- a stored falsy value must NOT trigger the default
                  stored = mem:get("k", "default-not-used"),
                  no_default = mem:get("nope") == nil,
                }
              end,
            }
        "#,
    )]);
    let host = host_with_mem();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["memory.read:proj/x", "memory.write:proj/x"], &["m.def"]),
            ActionCall {
                action: "m.def".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        res.value["missing"],
        serde_json::json!({ "fallback": true })
    );
    assert_eq!(res.value["stored"], false);
    assert_eq!(res.value["no_default"], true);
}

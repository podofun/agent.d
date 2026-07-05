//! `ctx.caller` — read-only identity view backed by the per-resume
//! `ActiveContext`. Covers field reads, nil fields, and write rejection.

use std::io::Write;
use std::sync::Arc;

use agentd_memory::MemMemoryStore;
use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_secrets::{MemoryStore, SecretStore};
use agentd_types::{ActionCall, CallContext, Registry};

fn write_init(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("init.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

async fn call(
    host: &LuaHost,
    caller: Caller,
    grants: &[&str],
    name: &str,
) -> Result<serde_json::Value, agentd_types::RegistryError> {
    host.call(
        CallContext {
            caller,
            effective_grants: PermissionSet::from_iter(grants.iter().copied()),
            call_chain: vec![name.to_string()],
            cwd: None,
        },
        ActionCall {
            action: name.to_string(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .map(|r| r.value)
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_fields_visible_in_handler() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "c" }
        agentd.action{
          name = "c.who",
          handler = function(_, ctx)
            return {
              interface = ctx.caller.interface,
              session   = ctx.caller.session,
              user      = ctx.caller.user,
              runner    = ctx.caller.runner == nil,
              service   = ctx.caller.service == nil,
            }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let caller = Caller::interface("ws")
        .with_session("ws-7")
        .with_user("tg-12345");
    let v = call(&host, caller, &[], "c.who").await.unwrap();
    assert_eq!(v["interface"], "ws");
    assert_eq!(v["session"], "ws-7");
    assert_eq!(v["user"], "tg-12345");
    assert_eq!(v["runner"], true);
    assert_eq!(v["service"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_absent_fields_are_nil() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "c" }
        agentd.action{
          name = "c.bare",
          handler = function(_, ctx)
            return {
              all_nil = ctx.caller.interface == nil
                and ctx.caller.session == nil
                and ctx.caller.user == nil
                and ctx.caller.runner == nil
                and ctx.caller.service == nil,
              unknown = ctx.caller.nonsense == nil,
            }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let v = call(&host, Caller::default(), &[], "c.bare").await.unwrap();
    assert_eq!(v["all_nil"], true);
    assert_eq!(v["unknown"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_is_read_only() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "c" }
        agentd.action{
          name = "c.mut",
          handler = function(_, ctx)
            ctx.caller.user = "spoofed"
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let err = call(&host, Caller::interface("ws"), &[], "c.mut")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("read-only"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn caller_drives_memory_namespace() {
    // The shiraz pattern, minus the external-id plumbing: memory ns derives
    // straight from ctx.caller.session.
    let dir = write_init(
        r#"
        agentd.tool{ name = "c" }
        agentd.action{
          name = "c.remember",
          handler = function(_, ctx)
            local mem = ctx.memory.create("chat/" .. ctx.caller.session)
            local n = mem:get("n", 0) + 1
            mem:set("n", n)
            return { n = n, ns = "chat/" .. ctx.caller.session }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_memory(Arc::new(MemMemoryStore::new()));
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let grants = ["memory.read:chat/**", "memory.write:chat/**"];
    let c = || Caller::interface("ws").with_session("ws-1");
    let v1 = call(&host, c(), &grants, "c.remember").await.unwrap();
    let v2 = call(&host, c(), &grants, "c.remember").await.unwrap();
    let other = Caller::interface("ws").with_session("ws-2");
    let v3 = call(&host, other, &grants, "c.remember").await.unwrap();
    assert_eq!(v1["n"], 1);
    assert_eq!(v2["n"], 2);
    assert_eq!(v3["n"], 1); // distinct session, distinct namespace
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_exists_reports_without_exposing() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "c" }
        agentd.action{
          name = "c.has",
          handler = function(_, ctx)
            return {
              present = ctx.secret.exists("openai_api_key"),
              absent  = ctx.secret.exists("missing_key"),
            }
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    let store = Arc::new(MemoryStore::new());
    store.set("openai_api_key", "sk-test").unwrap();
    host.set_secrets(store);
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let v = call(
        &host,
        Caller::interface("test"),
        &["secret:openai_api_key", "secret:missing_key"],
        "c.has",
    )
    .await
    .unwrap();
    assert_eq!(v["present"], true);
    assert_eq!(v["absent"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_exists_denied_without_grant() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "c" }
        agentd.action{
          name = "c.probe",
          handler = function(_, ctx)
            return ctx.secret.exists("openai_api_key")
          end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_secrets(Arc::new(MemoryStore::new()));
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let err = call(&host, Caller::interface("test"), &[], "c.probe")
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("secret:openai_api_key"),
        "got: {err}"
    );
}

//! `ctx.sessions` from Lua: create/open/get/turns/delete, permission gating,
//! and the optional `compact` field on `agentd.runner{...}`.

use std::io::Write;
use std::sync::Arc;

use agentd_permissions::{Caller, PermissionSet};
use agentd_runners::CompactPolicy;
use agentd_scripting::LuaHost;
use agentd_sessions::{MemSessionStore, NewSession, Scope, SessionStore};
use agentd_types::{ActionCall, CallContext, Registry};

fn write_init(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut f = std::fs::File::create(dir.path().join("init.lua")).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

async fn call(
    host: &LuaHost,
    grants: &[&str],
    name: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, agentd_types::RegistryError> {
    call_as(
        host,
        Caller::interface("ws").with_session("ws-1"),
        grants,
        name,
        args,
    )
    .await
}

async fn call_as(
    host: &LuaHost,
    caller: Caller,
    grants: &[&str],
    name: &str,
    args: serde_json::Value,
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
            args,
        },
    )
    .await
    .map(|r| r.value)
}

fn host_with(body: &str) -> (LuaHost, Arc<MemSessionStore>) {
    let dir = write_init(body);
    let host = LuaHost::new().unwrap();
    let store = Arc::new(MemSessionStore::new());
    host.set_sessions(store.clone());
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    std::mem::forget(dir);
    (host, store)
}

#[tokio::test(flavor = "multi_thread")]
async fn open_is_find_or_create_by_label() {
    let (host, store) = host_with(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{
          name = "s.open",
          handler = function(args, ctx)
            local a = ctx.sessions.open(args.label, { runner = "support" })
            local b = ctx.sessions.open(args.label)
            return { same = a.id == b.id, id = a.id, user = a.user, label = a.label,
                     found = ctx.sessions.find(args.label) ~= nil,
                     turns = ctx.sessions.turns(a.id) }
          end,
        }
        "#,
    );
    let v = call_as(
        &host,
        Caller::interface("ws").with_user("alice"),
        &["sessions.read", "sessions.write"],
        "s.open",
        serde_json::json!({ "label": "tg-7" }),
    )
    .await
    .unwrap();
    assert_eq!(v["same"], true);
    // `user` comes from the caller, never from the options table.
    assert_eq!(v["user"], "alice");
    assert_eq!(v["label"], "tg-7");
    assert_eq!(v["found"], true);
    assert_eq!(v["turns"], serde_json::json!([]));
    let id = v["id"].as_str().unwrap();
    assert!(store.get(id).unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_need_sessions_write_grant() {
    let (host, _store) = host_with(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{ name = "s.new", handler = function(_, ctx) return ctx.sessions.create() end }
        agentd.action{ name = "s.ls", handler = function(_, ctx) return ctx.sessions.list() end }
        "#,
    );
    let denied = call(&host, &["sessions.read"], "s.new", serde_json::Value::Null).await;
    assert!(denied.is_err(), "create without sessions.write must fail");
    let ok = call(&host, &["sessions.write"], "s.new", serde_json::Value::Null).await;
    assert!(ok.is_ok(), "{ok:?}");
    let denied = call(&host, &["sessions.write"], "s.ls", serde_json::Value::Null).await;
    assert!(denied.is_err(), "list without sessions.read must fail");
    let ls = call(&host, &["sessions.read"], "s.ls", serde_json::Value::Null)
        .await
        .unwrap();
    assert_eq!(ls.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_returns_bool() {
    let (host, store) = host_with(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{ name = "s.rm", handler = function(args, ctx) return { gone = ctx.sessions.delete(args.id) } end }
        "#,
    );
    let m = store
        .create(NewSession::in_scope(&Scope::from_caller(
            Some("ws"),
            None,
            None,
        )))
        .unwrap();
    let v = call(
        &host,
        &["sessions.write"],
        "s.rm",
        serde_json::json!({ "id": m.id }),
    )
    .await
    .unwrap();
    assert_eq!(v["gone"], true);
    let v = call(
        &host,
        &["sessions.write"],
        "s.rm",
        serde_json::json!({ "id": m.id }),
    )
    .await
    .unwrap();
    assert_eq!(v["gone"], false);
}

#[test]
fn compact_field_is_optional_and_partial() {
    let dir = write_init(
        r#"
        agentd.runner({ name = "plain", model = "mock/x" })
        agentd.runner({ name = "off", model = "mock/x", compact = false })
        agentd.runner({ name = "tuned", model = "mock/x", compact = { after_turns = 10, keep_recent = 4 } })
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let r = host.runners();
    assert_eq!(r.get("plain").unwrap().compact, None);
    assert_eq!(
        r.get("off").unwrap().compact,
        Some(CompactPolicy {
            enabled: false,
            ..Default::default()
        })
    );
    assert_eq!(
        r.get("tuned").unwrap().compact,
        Some(CompactPolicy {
            after_turns: 10,
            keep_recent: 4,
            ..Default::default()
        })
    );
}

#[test]
fn compact_rejects_unknown_fields_and_bad_bounds() {
    for body in [
        r#"agentd.runner({ name = "a", model = "mock/x", compact = { max_turns = 3 } })"#,
        r#"agentd.runner({ name = "b", model = "mock/x", compact = { after_turns = 5, keep_recent = 5 } })"#,
        r#"agentd.runner({ name = "c", model = "mock/x", compact = "yes" })"#,
    ] {
        let dir = write_init(body);
        let host = LuaHost::new().unwrap();
        host.set_root(dir.path());
        let err = host.load_file(&dir.path().join("init.lua")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("compact"), "{msg}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scripts_only_see_their_callers_sessions() {
    let (host, store) = host_with(
        r#"
        agentd.tool{ name = "s" }
        agentd.action{ name = "s.peek", handler = function(args, ctx)
          return { meta = ctx.sessions.get(args.id), n = #ctx.sessions.list(),
                   by_label = ctx.sessions.find(args.label) }
        end }
        agentd.action{ name = "s.turns", handler = function(args, ctx) return ctx.sessions.turns(args.id) end }
        agentd.action{ name = "s.new", handler = function(args, ctx)
          return ctx.sessions.create({ user = "eve" })
        end }
        "#,
    );
    let alice = store
        .create(
            NewSession::in_scope(&Scope::from_caller(Some("webapp"), None, Some("alice")))
                .label(Some("chat-1".into())),
        )
        .unwrap();
    let grants = ["sessions.read", "sessions.write"];
    let args = serde_json::json!({ "id": alice.id, "label": "chat-1" });
    // Other interface, same user name: invisible.
    let v = call_as(
        &host,
        Caller::interface("telegram").with_user("alice"),
        &grants,
        "s.peek",
        args.clone(),
    )
    .await
    .unwrap();
    assert!(v["meta"].is_null() && v["by_label"].is_null());
    assert_eq!(v["n"], 0);
    // Same interface, other user: invisible; turns() reports missing.
    let v = call_as(
        &host,
        Caller::interface("webapp").with_user("bob"),
        &grants,
        "s.peek",
        args.clone(),
    )
    .await
    .unwrap();
    assert!(v["meta"].is_null());
    let err = call_as(
        &host,
        Caller::interface("webapp").with_user("bob"),
        &grants,
        "s.turns",
        args.clone(),
    )
    .await;
    assert!(err.is_err());
    // The owner sees it.
    let v = call_as(
        &host,
        Caller::interface("webapp").with_user("alice"),
        &grants,
        "s.peek",
        args,
    )
    .await
    .unwrap();
    assert_eq!(v["meta"]["id"], alice.id);
    assert_eq!(v["by_label"]["id"], alice.id);
    // Scripts cannot mint sessions for an arbitrary user.
    let err = call(&host, &grants, "s.new", serde_json::Value::Null).await;
    assert!(err.is_err(), "create with a user option must be refused");
}

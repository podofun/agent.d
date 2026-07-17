//! context.auth + context.ai (P3) integration tests.

use std::io::Write;
use std::sync::Arc;

use agentd_ai::MockProvider;
use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_secrets::{MemoryStore, SecretStore};
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
        cwd: None,
    }
}

fn make_host_with_backends() -> LuaHost {
    let host = LuaHost::new().unwrap();
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    host.set_secrets(secrets);
    host.set_ai_provider("mock", Arc::new(MockProvider::new().with_reply("pong")));
    host.set_default_ai_provider("mock");
    host
}

// ---------- auth ----------

#[tokio::test(flavor = "multi_thread")]
async fn auth_set_get_roundtrip_with_grant() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "a.rt",
          handler = function(_, ctx)
            ctx.secret.set("openai_api_key", "sk-secret")
            return { v = ctx.secret.get("openai_api_key") }
          end,
        }
        "#,
    )]);
    let host = make_host_with_backends();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["secret:openai_api_key"]),
            ActionCall {
                action: "a.rt".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        res.value.get("v").and_then(|v| v.as_str()),
        Some("sk-secret")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_get_denied_without_grant() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "a.get",
          handler = function(_, ctx)
            return { v = ctx.secret.get("anything") }
          end,
        }
        "#,
    )]);
    let host = make_host_with_backends();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[]),
            ActionCall {
                action: "a.get".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("secret:"), "got {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_wildcard_grant_covers_all_keys() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "a.many",
          handler = function(_, ctx)
            ctx.secret.set("a", "1")
            ctx.secret.set("b", "2")
            return { a = ctx.secret.get("a"), b = ctx.secret.get("b") }
          end,
        }
        "#,
    )]);
    let host = make_host_with_backends();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["secret:*"]),
            ActionCall {
                action: "a.many".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("a").and_then(|v| v.as_str()), Some("1"));
    assert_eq!(res.value.get("b").and_then(|v| v.as_str()), Some("2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_prefix_wildcard_grant() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "a.pref",
          handler = function(_, ctx)
            ctx.secret.set("openai_api_key", "x")
            return { v = ctx.secret.get("openai_api_key") }
          end,
        }
        "#,
    )]);
    let host = make_host_with_backends();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["secret:openai_*"]),
            ActionCall {
                action: "a.pref".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("v").and_then(|v| v.as_str()), Some("x"));
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_delete_removes_value() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "a.del",
          handler = function(_, ctx)
            ctx.secret.set("k", "v")
            ctx.secret.delete("k")
            local ok, err = pcall(function()
              return ctx.secret.get("k")
            end)
            return { ok = ok, err = tostring(err) }
          end,
        }
        "#,
    )]);
    let host = make_host_with_backends();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["secret:k"]),
            ActionCall {
                action: "a.del".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("ok").and_then(|v| v.as_bool()), Some(false));
    let err = res.value.get("err").and_then(|v| v.as_str()).unwrap();
    assert!(err.contains("no secret named"), "got: {err}");
}

// ---------- ai ----------

#[tokio::test(flavor = "multi_thread")]
async fn ai_ask_uses_default_provider_with_grant() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "ai.go",
          handler = function(_, ctx)
            local r = ctx.ai.ask("hello")
            return { text = r.text, provider = r.provider }
          end,
        }
        "#,
    )]);
    let host = make_host_with_backends();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["ai:mock"]),
            ActionCall {
                action: "ai.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value.get("text").and_then(|v| v.as_str()), Some("pong"));
    assert_eq!(
        res.value.get("provider").and_then(|v| v.as_str()),
        Some("mock")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_ask_denied_without_grant() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "ai.go",
          handler = function(_, ctx)
            return ctx.ai.ask("hi")
          end,
        }
        "#,
    )]);
    let host = make_host_with_backends();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[]),
            ActionCall {
                action: "ai.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("ai:"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_complete_passes_system_and_messages() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "ai.complete",
          handler = function(_, ctx)
            local r = ctx.ai.complete{
              system = "be terse",
              messages = {
                { role = "user", content = "ping" },
              },
              prompt = "again",
            }
            return { text = r.text }
          end,
        }
        "#,
    )]);
    // Use the echoing mock (no with_reply).
    let host = LuaHost::new().unwrap();
    host.set_ai_provider("echo", Arc::new(MockProvider::new()));
    host.set_default_ai_provider("echo");
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["ai:echo"]),
            ActionCall {
                action: "ai.complete".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let text = res.value.get("text").and_then(|v| v.as_str()).unwrap();
    assert!(text.contains("be terse"));
    assert!(text.contains("[user] ping"));
    assert!(text.contains("again"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_choose_provider_via_model_prefix() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "ai.alt",
          handler = function(_, ctx)
            local r = ctx.ai.ask("x", { model = "alt/whatever" })
            return { text = r.text, provider = r.provider }
          end,
        }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.set_ai_provider("alt", Arc::new(MockProvider::new().with_reply("from-alt")));
    host.set_ai_provider(
        "default",
        Arc::new(MockProvider::new().with_reply("from-default")),
    );
    host.set_default_ai_provider("default");
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["ai:alt"]),
            ActionCall {
                action: "ai.alt".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        res.value.get("text").and_then(|v| v.as_str()),
        Some("from-alt")
    );
    assert_eq!(
        res.value.get("provider").and_then(|v| v.as_str()),
        Some("alt")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ai_providers_lists_registered() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "ai.who",
          handler = function(_, ctx)
            return { list = ctx.ai.providers() }
          end,
        }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.set_ai_provider("a", Arc::new(MockProvider::new()));
    host.set_ai_provider("b", Arc::new(MockProvider::new()));
    host.set_default_ai_provider("a");
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["ai:a"]),
            ActionCall {
                action: "ai.who".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let list = res.value.get("list").and_then(|v| v.as_array()).unwrap();
    let names: Vec<&str> = list.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
}

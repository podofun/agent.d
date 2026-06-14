//! Bare global `json.encode(v)` / `json.decode(s)`.

use std::io::Write;
use std::sync::Arc;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_secrets::MemoryStore;
use agentd_types::{ActionCall, Registry};

fn host_with(root: &std::path::Path) -> Arc<LuaHost> {
    let host = LuaHost::new().unwrap();
    host.set_root(root);
    host.set_secrets(Arc::new(MemoryStore::default()));
    Arc::new(host)
}

fn write_init(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("init.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn encode_decode_round_trip() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "j" }
        agentd.action{
          name = "j.run",
          handler = function(_, ctx)
            local s = json.encode({ a = 1, b = "two", c = { 3, 4 } })
            local d = json.decode(s)
            return { s = s, a = d.a, b = d.b, c1 = d.c[1] }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let caller = Caller::default();
    let res = host
        .call(
            agentd_types::CallContext {
                caller,
                effective_grants: PermissionSet::empty(),
                call_chain: vec!["j.run".into()],
            },
            ActionCall {
                action: "j.run".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value["a"], 1);
    assert_eq!(res.value["b"], "two");
    assert_eq!(res.value["c1"], 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn decode_invalid_errors() {
    let dir = write_init(
        r#"
        agentd.tool{ name = "j" }
        agentd.action{
          name = "j.bad",
          handler = function(_, ctx)
            local ok, err = pcall(json.decode, "not json")
            return { ok = ok, err = tostring(err) }
          end,
        }
        "#,
    );
    let host = host_with(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
    let res = host
        .call(
            agentd_types::CallContext {
                caller: Caller::default(),
                effective_grants: PermissionSet::empty(),
                call_chain: vec!["j.bad".into()],
            },
            ActionCall {
                action: "j.bad".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value["ok"], false);
}

//! Live registry handles: registration returns the item, properties read
//! and write the registry, and every collection can get, find and remove.

use std::io::Write;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};

fn load(source: &str) -> LuaHost {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("init.lua");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&path).unwrap();
    host
}

async fn call(host: &LuaHost, action: &str) -> serde_json::Value {
    host.call(
        CallContext {
            caller: Caller::interface("test"),
            effective_grants: PermissionSet::default(),
            call_chain: vec![action.to_owned()],
            cwd: None,
        },
        ActionCall {
            action: action.into(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .unwrap()
    .value
}

#[tokio::test]
async fn later_registration_replaces_earlier_for_every_kind() {
    let host = load(
        r#"
        agentd.runner{ name = "r", model = "mock/a" }
        agentd.runner{ name = "r", model = "mock/b" }
        agentd.skill{ name = "s", system = "first" }
        agentd.skill{ name = "s", system = "second" }
        agentd.tool{ name = "t", requires = { "fs.read:a" } }
        agentd.tool{ name = "t", requires = { "fs.read:b" } }
        agentd.action("x", function() return 1 end)
        agentd.action("x", function() return 2 end)
        agentd.service("svc", { restart = "always" }, function() end)
        agentd.service("svc", { restart = "on_failure" }, function() end)
        "#,
    );
    assert_eq!(
        host.runners().get("r").unwrap().model.as_deref(),
        Some("mock/b")
    );
    assert_eq!(host.skills().get("s").unwrap().system, "second");
    assert_eq!(host.tool_info("t").unwrap().requires, ["fs.read:b"]);
    assert_eq!(call(&host, "x").await, 2);
    assert_eq!(
        host.services().get("svc").unwrap().restart.as_deref(),
        Some("on_failure")
    );
}

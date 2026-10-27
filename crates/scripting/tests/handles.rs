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
        r##"
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
        "##,
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

#[test]
fn registration_returns_a_live_handle() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("skills")).unwrap();
    std::fs::write(
        dir.path().join("skills/review.md"),
        "---\nname: review\ndescription: Review a diff\n---\nLook closely.\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("init.lua"),
        r##"
        local r = agentd.runner{ name = "coder", model = "mock/a", skills = { "s" }, actions = { "x" } }
        assert(r.model == "mock/a", "model")
        assert(r.skills[1] == "s", "skills[1]")
        assert(#r.skills == 1, "#skills")
        assert(r.allowed_actions[1] == "x", "allowed_actions")
        assert(r.actions[1] == "x", "actions alias")
        assert(r.cwd == nil, "absent options read as nil")
        assert(tostring(r) == "runner coder", tostring(r))
        local seen = {}
        for i, skill in ipairs(r.skills) do seen[i] = skill end
        assert(seen[1] == "s" and seen[2] == nil, "ipairs")

        local a = agentd.action("x", function() return 1 end)
        assert(a.name == "x", "action name")
        assert(type(a.handler) == "function", "handler")
        assert(agentd.action{ name = "y.z", handler = function() end }.tool == "y", "inferred tool")
        assert(agentd.tool{ name = "t", requires = { "fs.read:a" } }.requires[1] == "fs.read:a", "tool")
        assert(agentd.skill{ name = "k", system = "Be brief." }.system == "Be brief.", "skill")
        assert(agentd.service("svc", { restart = "always" }, function() end).restart == "always", "service")

        local loaded = agentd.skills.dir("skills")
        assert(#loaded == 1 and loaded[1].name == "review", "skills.dir")
        assert(loaded[1].description == "Review a diff", "skill description")
        assert(agentd.skills.load("skills/review.md").name == "review", "skills.load")
        "##,
    )
    .unwrap();
    let host = LuaHost::new().unwrap();
    host.set_root(dir.path());
    host.load_file(&dir.path().join("init.lua")).unwrap();
}

#[test]
fn get_returns_the_same_item() {
    load(
        r##"
        local r = agentd.runner{ name = "coder", model = "mock/a" }
        assert(agentd.runners.get("coder") == r, "same handle")
        assert(agentd.runners.get("ghost") == nil, "missing")
        local a = agentd.action("x", function() end)
        assert(agentd.actions.get("x") == a, "action")
        agentd.tool{ name = "t" }
        assert(agentd.tools.get("t").name == "t", "tool")
        agentd.skill{ name = "k" }
        assert(agentd.skills.get("k").name == "k", "skill")
        agentd.service("svc", function() end)
        assert(agentd.services.get("svc").name == "svc", "service")
        "##,
    );
}

#[test]
fn stale_handles_say_why() {
    load(
        r##"
        local old = agentd.runner{ name = "c", model = "mock/a" }
        agentd.runner{ name = "c", model = "mock/b" }
        local ok, err = pcall(function() return old.model end)
        assert(not ok, "stale read must fail")
        assert(tostring(err):find("`c` was replaced by a later registration", 1, true), tostring(err))
        assert(agentd.runners.get("c").model == "mock/b", "new item")
        "##,
    );
}

#[test]
fn unknown_property_reads_error() {
    load(
        r##"
        local r = agentd.runner{ name = "coder", model = "mock/a" }
        local ok, err = pcall(function() return r.modle end)
        assert(not ok, "typo must fail")
        assert(tostring(err):find("the runner `coder` has no property `modle`", 1, true), tostring(err))
        "##,
    );
}

#[test]
fn package_names_are_scoped() {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("acme");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.toml"),
        "[package]\nname=\"acme\"\nentry=\"main.lua\"\n",
    )
    .unwrap();
    std::fs::write(
        package.join("main.lua"),
        r##"
        local ping = agentd.action("ping", function() return {} end)
        assert(ping.name == "acme/ping", ping.name)
        "##,
    )
    .unwrap();
    let init = root.path().join("init.lua");
    std::fs::write(
        &init,
        r##"
        import("acme")
        assert(agentd.actions.get("acme/ping").name == "acme/ping", "scoped get")
        assert(agentd.actions.get("ping") == nil, "unscoped name is not registered")
        "##,
    )
    .unwrap();
    let host = LuaHost::new().unwrap();
    host.set_root(root.path());
    host.set_packages_root(root.path());
    host.load_file(&init).unwrap();
}

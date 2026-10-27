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

#[test]
fn assignment_changes_the_registered_item() {
    let host = load(
        r##"
        local r = agentd.runner{ name = "coder", model = "mock/a" }
        r.model = "mock/b"
        r.system = "Be careful."
        r.cwd = nil
        "##,
    );
    let runner = host.runners().get("coder").unwrap();
    assert_eq!(runner.model.as_deref(), Some("mock/b"));
    assert_eq!(runner.system.as_deref(), Some("Be careful."));
}

#[test]
fn lists_are_live() {
    let host = load(
        r##"
        local r = agentd.runner{ name = "coder", skills = { "s" } }
        table.insert(r.skills, "tdd")
        assert(r.skills[2] == "tdd" and #r.skills == 2, "insert")
        r.skills[1] = "debug"
        assert(r.skills[1] == "debug", "set")
        table.remove(r.skills, 1)
        assert(#r.skills == 1 and r.skills[1] == "tdd", "remove")
        local other = agentd.runner{ name = "other", skills = { "x", "y" } }
        r.skills = other.skills
        assert(r.skills[2] == "y", "copy from another list")
        r.skills = { "a", "b", "c" }
        local ok, err = pcall(function() r.skills[9] = "far" end)
        assert(not ok and tostring(err):find("index 9", 1, true), tostring(err))
        local ok2, err2 = pcall(function() r.skills[1] = 5 end)
        assert(not ok2 and tostring(err2):find("string", 1, true), tostring(err2))
        "##,
    );
    assert_eq!(host.runners().get("coder").unwrap().skills, ["a", "b", "c"]);
}

#[test]
fn bad_assignments_fail_and_change_nothing() {
    let host = load(
        r##"
        local r = agentd.runner{ name = "coder", model = "mock/a" }
        local ok, err = pcall(function() r.modle = "x" end)
        assert(not ok and tostring(err):find("the runner `coder` has no property `modle`", 1, true), tostring(err))
        ok, err = pcall(function() r.model = 5 end)
        assert(not ok and tostring(err):find("`model`", 1, true), tostring(err))
        ok, err = pcall(function() r.compact = { keep_recent = 99, after_turns = 10 } end)
        assert(not ok and tostring(err):find("keep_recent", 1, true), tostring(err))
        assert(r.model == "mock/a", "unchanged after failures")
        r.compact = false
        assert(r.compact.enabled == false, "compact off")
        "##,
    );
    assert_eq!(
        host.runners().get("coder").unwrap().model.as_deref(),
        Some("mock/a")
    );
}

#[test]
fn rename_keeps_the_handle_and_guards_references() {
    let host = load(
        r##"
        local r = agentd.runner{ name = "coder", actions = { "x" }, skills = { "k" } }
        r.name = "coder2"
        assert(agentd.runners.get("coder2") == r, "same handle")
        assert(agentd.runners.get("coder") == nil, "old name gone")
        assert(tostring(r) == "runner coder2", tostring(r))

        local x = agentd.action("x", function() end)
        local ok, err = pcall(function() x.name = "y" end)
        assert(not ok and tostring(err):find("`coder2`", 1, true), tostring(err))

        local k = agentd.skill{ name = "k" }
        ok, err = pcall(function() k.name = "k2" end)
        assert(not ok and tostring(err):find("`coder2`", 1, true), tostring(err))

        agentd.tool{ name = "git" }
        agentd.action("git.status", function() end)
        ok, err = pcall(function() agentd.tools.get("git").name = "vcs" end)
        assert(not ok and tostring(err):find("`git.status`", 1, true), tostring(err))

        agentd.runner{ name = "taken" }
        ok, err = pcall(function() r.name = "taken" end)
        assert(not ok and tostring(err):find("already", 1, true), tostring(err))

        local free = agentd.action("free", function() return 7 end)
        free.name = "freed"
        assert(agentd.actions.get("freed") == free and agentd.actions.get("free") == nil, "action rename")
        "##,
    );
    assert!(host.runners().get("coder").is_none());
    assert!(host.list().iter().any(|name| name == "freed"));
}

#[tokio::test]
async fn action_handler_and_schema_can_change() {
    let host = load(
        r##"
        local a = agentd.action{ name = "x", handler = function() return 1 end }
        a.handler = function() return 2 end
        a.confirm = true
        assert(a.confirm == true, "confirm")
        local s = agentd.service("svc", function() end)
        local replacement = function() end
        s.handler = replacement
        assert(s.handler == replacement, "service handler")
        "##,
    );
    assert_eq!(call(&host, "x").await, 2);

    let host = load(
        r##"
        local a = agentd.action{ name = "x", handler = function() return 1 end }
        a.input = { n = { type = "integer", required = true } }
        "##,
    );
    let error = host
        .call(
            CallContext {
                caller: Caller::interface("test"),
                effective_grants: PermissionSet::default(),
                call_chain: vec!["x".into()],
                cwd: None,
            },
            ActionCall {
                action: "x".into(),
                args: serde_json::json!({}),
            },
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("the required field `/n` is missing"),
        "{error}"
    );
}

#[test]
fn find_by_table_and_function() {
    load(
        r##"
        agentd.action("git.status", function() end)
        agentd.action("git.diff", function() end)
        agentd.action("fs.read", function() end)
        local git = agentd.actions.find({ name = "git.*" })
        assert(#git == 2 and git[1].name == "git.diff" and git[2].name == "git.status", "glob, sorted")
        local read = agentd.actions.find(function(a) return a.name == "fs.read" end)
        assert(#read == 1 and read[1] == agentd.actions.get("fs.read"), "predicate returns handles")
        agentd.runner{ name = "a", model = "mock/x", skills = { "debug", "tdd" } }
        agentd.runner{ name = "b", model = "mock/y", skills = { "debug" } }
        assert(#agentd.runners.find({ skills = "tdd" }) == 1, "list field")
        assert(#agentd.runners.find({ skills = "debug", model = "mock/y" }) == 1, "all keys must match")
        assert(#agentd.runners.find({ model = "mock/*" }) == 2, "glob on another field")
        assert(#agentd.runners.find({}) == 2, "empty query matches all")
        local ok, err = pcall(agentd.runners.find, { modle = "x" })
        assert(not ok and tostring(err):find("`modle`", 1, true), tostring(err))
        assert(#agentd.skills.find({ name = "*" }) == 0, "no skills")
        "##,
    );
}

#[test]
fn remove_unregisters_and_retires() {
    let host = load(
        r##"
        local s = agentd.skill{ name = "k", system = "x" }
        s:remove()
        assert(agentd.skills.get("k") == nil, "skill gone")
        local ok, err = pcall(function() return s.system end)
        assert(not ok and tostring(err):find("the skill `k` was removed", 1, true), tostring(err))
        agentd.tool{ name = "t" }
        agentd.tools.remove("t")
        assert(agentd.tools.get("t") == nil, "tool gone")
        agentd.runner{ name = "r" }
        agentd.runners.remove("r")
        local svc = agentd.service("svc", function() end)
        svc:remove()
        agentd.action("x", function() end)
        agentd.actions.remove("x")
        assert(#agentd.actions.list() == 0, "action gone")
        ok, err = pcall(agentd.skills.remove, "ghost")
        assert(not ok and tostring(err):find("there is no skill named `ghost`", 1, true), tostring(err))
        "##,
    );
    assert!(host.runners().get("r").is_none());
    assert!(host.services().get("svc").is_none());
    assert!(host.skills().get("k").is_none());
}

#[test]
fn remove_refuses_while_referenced() {
    load(
        r##"
        agentd.runner{ name = "coder", actions = { "x" } }
        agentd.action("x", function() end)
        local ok, err = pcall(agentd.actions.remove, "x")
        assert(not ok and tostring(err):find("`coder` still lists `x`", 1, true), tostring(err))
        table.remove(agentd.runners.get("coder").allowed_actions, 1)
        agentd.actions.remove("x")
        assert(agentd.actions.get("x") == nil, "removed after the runner let go")
        "##,
    );
}

fn load_with_package(init: &str, package_main: &str) -> LuaHost {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("acme");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("package.toml"),
        "[package]\nname=\"acme\"\nentry=\"main.lua\"\n",
    )
    .unwrap();
    std::fs::write(package.join("main.lua"), package_main).unwrap();
    let path = root.path().join("init.lua");
    std::fs::write(&path, init).unwrap();
    let host = LuaHost::new().unwrap();
    host.set_root(root.path());
    host.set_packages_root(root.path());
    host.load_file(&path).unwrap();
    host
}

#[test]
fn packages_cannot_change_what_they_do_not_own() {
    let host = load_with_package(
        r##"
        agentd.tool{ name = "git" }
        agentd.action("git.status", function() end)
        agentd.skill{ name = "k" }
        import("acme")
        "##,
        r##"
        local function denied(f, what)
          local ok, err = pcall(f)
          assert(not ok, what .. " must be refused")
          assert(tostring(err):find("the package `acme` cannot change", 1, true), tostring(err))
        end
        denied(function() agentd.actions.remove("git.status") end, "remove")
        denied(function() agentd.actions.get("git.status").confirm = true end, "assign")
        denied(function() agentd.tools.get("git").name = "vcs" end, "rename")
        denied(function() agentd.skills.get("k").system = "x" end, "skill")

        local a = agentd.action("x", function() end)
        a.name = "git.status2"
        assert(a.name == "acme/git.status2", "rename target is scoped: " .. a.name)
        a.tool = "git"
        assert(a.tool == "acme/git", "tool is scoped: " .. tostring(a.tool))
        local own = agentd.skill{ name = "pk" }
        own.system = "mine"
        assert(own.system == "mine", "a package may change its own skill")
        "##,
    );
    assert_eq!(
        host.action_info("git.status").unwrap().tool.as_deref(),
        Some("git")
    );
}

#[test]
fn package_ownership_follows_renames_and_removals() {
    let host = load_with_package(
        r##"import("acme")"##,
        r##"
        local poller = agentd.service("poller", function() end)
        poller.name = "poller2"
        agentd.runner{ name = "r" }
        agentd.runners.remove("acme/r")
        "##,
    );
    let packages = host.loaded_packages();
    let acme = packages.iter().find(|p| p.name == "acme").unwrap();
    assert_eq!(acme.services, ["acme/poller2"]);
    assert!(acme.runners.is_empty(), "{:?}", acme.runners);
}

#[test]
fn assignment_normalizes_like_registration() {
    load(
        r##"
        local a = agentd.action{ name = "y.z", handler = function() end, confirm = true }
        a.confirm = nil
        assert(a.confirm == false, "nil confirm is false")
        a.tool = "other"
        a.tool = nil
        assert(a.tool == "y", "nil tool is inferred from the name: " .. tostring(a.tool))
        local s = agentd.skill{ name = "k", system = "x" }
        s.system = "  trimmed  "
        assert(s.system == "trimmed", "system is trimmed")
        s.system = nil
        assert(s.system == "", "nil system is empty")
        local r = agentd.runner{ name = "coder" }
        local ok, err = pcall(function() r.compact = 5 end)
        assert(not ok and tostring(err):find("the runner `coder`", 1, true), tostring(err))
        ok, err = pcall(function() r.compact = { keep_recent = 9, after_turns = 3 } end)
        assert(not ok and tostring(err):find("the runner `coder`", 1, true), tostring(err))
        "##,
    );
}

#[test]
fn pairs_iterates_list_properties() {
    load(
        r##"
        local r = agentd.runner{ name = "coder", skills = { "a", "b" } }
        local seen = {}
        for i, skill in pairs(r.skills) do seen[#seen + 1] = i .. "=" .. skill end
        assert(table.concat(seen, ",") == "1=a,2=b", table.concat(seen, ","))
        "##,
    );
}

#[test]
fn errors_name_the_item_in_plain_words() {
    load(
        r##"
        local old = agentd.runner{ name = "c" }
        agentd.runner{ name = "c" }
        local ok, err = pcall(function() return old.modle end)
        assert(not ok and tostring(err):find("`c`", 1, true), tostring(err))

        local r = agentd.runner{ name = "r", skills = { "a" } }
        ok, err = pcall(function() r.model = r.skills end)
        assert(not ok and tostring(err):find("the runner `r` cannot take that value for `model`", 1, true), tostring(err))
        ok, err = pcall(function() return r.skills.foo end)
        assert(not ok and tostring(err):find("the runner `r` list `skills` is indexed by number", 1, true), tostring(err))
        ok, err = pcall(function() r.skills.foo = "x" end)
        assert(not ok and tostring(err):find("is indexed by number", 1, true), tostring(err))
        "##,
    );
}

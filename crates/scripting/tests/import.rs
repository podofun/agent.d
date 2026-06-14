//! `import` sandbox: root-relative resolution, refusal of absolute /
//! `..` paths, dedupe of repeat imports, and unset-root failure.

use std::fs;
use std::io::Write;

use agentd_scripting::LuaHost;

fn write(root: &std::path::Path, rel: &str, body: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let mut f = fs::File::create(&path).unwrap();
    f.write_all(body.as_bytes()).unwrap();
}

#[test]
fn import_runs_root_relative_file() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "tools/ping.lua",
        r#"agentd.action("ping", function(_, ctx) return { ok = true } end)"#,
    );
    write(tmp.path(), "init.lua", r#"import("tools/ping.lua")"#);

    let host = LuaHost::new().unwrap();
    host.set_root(tmp.path());
    host.load_file(&tmp.path().join("init.lua")).unwrap();
    assert!(host.runners().is_empty(), "ping is an action, not a runner");
    use agentd_types::Registry;
    assert!(host.list().iter().any(|n| n == "ping"));
}

#[test]
fn import_dedupes_repeat_calls() {
    let tmp = tempfile::tempdir().unwrap();
    // side.lua registers an action; if it ran twice and used a duplicate name
    // we'd just overwrite, but if dedupe is broken we'd see an error chain on
    // a `return` value mismatch. Use the explicit return-value cache: first
    // import returns the chunk's value, second returns `true` because the
    // chunk wasn't re-executed.
    write(tmp.path(), "side.lua", r#"return { hello = "world" }"#);
    write(
        tmp.path(),
        "init.lua",
        r#"
        local a = import("side.lua")
        local b = import("side.lua")
        assert(type(a) == "table", "first import should return the module table")
        assert(a.hello == "world")
        assert(b == true, "second import should be a deduped no-op returning `true`")
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(tmp.path());
    host.load_file(&tmp.path().join("init.lua")).unwrap();
}

#[test]
fn import_rejects_parent_traversal() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "init.lua", r#"import("../escape.lua")"#);
    let host = LuaHost::new().unwrap();
    host.set_root(tmp.path());
    let err = host.load_file(&tmp.path().join("init.lua")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains(".."), "expected `..` rejection error: {msg}");
}

#[test]
fn import_rejects_absolute_paths() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "init.lua", r#"import("/etc/passwd")"#);
    let host = LuaHost::new().unwrap();
    host.set_root(tmp.path());
    let err = host.load_file(&tmp.path().join("init.lua")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("absolute"),
        "expected absolute rejection: {msg}"
    );
}

#[test]
fn import_without_root_errors() {
    let tmp = tempfile::tempdir().unwrap();
    write(tmp.path(), "init.lua", r#"import("foo.lua")"#);
    let host = LuaHost::new().unwrap();
    // Note: no set_root call.
    let err = host.load_file(&tmp.path().join("init.lua")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("root"), "expected root-missing error: {msg}");
}

#[test]
fn skills_load_dir_walks_markdown() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "skills/reviewer.md",
        "---\nname: reviewer\nactions:\n  - git.diff\n---\nbody\n",
    );
    write(tmp.path(), "init.lua", r#"agentd.skills.dir("skills")"#);
    let host = LuaHost::new().unwrap();
    host.set_root(tmp.path());
    host.load_file(&tmp.path().join("init.lua")).unwrap();
    assert!(host.skills().get("reviewer").is_some());
}

#[test]
fn inline_skill_registers() {
    let tmp = tempfile::tempdir().unwrap();
    write(
        tmp.path(),
        "init.lua",
        r#"
        agentd.skill({
            name = "terse",
            system = "Be terse.",
            actions = { "git.diff" },
        })
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.set_root(tmp.path());
    host.load_file(&tmp.path().join("init.lua")).unwrap();
    let s = host.skills().get("terse").unwrap();
    assert_eq!(s.system, "Be terse.");
    assert_eq!(s.actions, vec!["git.diff"]);
}

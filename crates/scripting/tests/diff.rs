//! `ctx.diff`: create, parse, format and apply unified diffs from Lua.

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};

async fn run(script: &str) -> Result<serde_json::Value, String> {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("tool.lua");
    std::fs::write(
        &source,
        format!("agentd.action(\"test\", function(_, ctx)\n{script}\nend)"),
    )
    .unwrap();
    let host = LuaHost::new().unwrap();
    host.set_workspace_root(dir.path());
    host.load_file(&source).unwrap();
    host.call(
        CallContext {
            caller: Caller::interface("test"),
            effective_grants: PermissionSet::default(),
            call_chain: Vec::new(),
            cwd: None,
        },
        ActionCall {
            action: "test".into(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .map(|result| result.value)
    .map_err(|error| error.to_string())
}

#[tokio::test]
async fn lua_creates_parses_and_applies() {
    let value = run(r#"
        local d = ctx.diff.create("a\nb\n", "a\nc\n", { path = "x.txt" })
        local p = ctx.diff.parse(d)
        assert(p[1].new_path == "x.txt", "path")
        assert(p[1].hunks[1].lines[2] == "-b", "lines")
        return ctx.diff.apply("a\nb\n", d)
    "#)
    .await
    .unwrap();
    assert_eq!(value, "a\nc\n");
}

#[tokio::test]
async fn lua_can_edit_a_parsed_patch() {
    let value = run(r#"
        local p = ctx.diff.parse(ctx.diff.create("a\nb\n", "a\nc\n", { path = "x.txt" }))
        p[1].hunks[1].lines[3] = "+z"
        local text = ctx.diff.format(p)
        assert(text:find("+z", 1, true), text)
        return ctx.diff.apply("a\nb\n", p[1])
    "#)
    .await
    .unwrap();
    assert_eq!(value, "a\nz\n");
}

#[tokio::test]
async fn created_files_parse_with_nil_old_path() {
    let value = run(r#"
        local p = ctx.diff.parse("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n")
        assert(p[1].old_path == nil, "dev null is nil")
        return ctx.diff.apply("", p[1])
    "#)
    .await
    .unwrap();
    assert_eq!(value, "hi\n");
}

#[tokio::test]
async fn apply_errors_are_sentences() {
    let error = run(r#"
        local d = ctx.diff.create("a\nb\n", "a\nc\n", { path = "x.txt" })
        return ctx.diff.apply("q\n", d)
    "#)
    .await
    .unwrap_err();
    assert!(error.contains("does not apply"), "{error}");
    let error = run(r#"
        return ctx.diff.apply("a\n", "--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-a\n+b\n")
    "#)
    .await
    .unwrap_err();
    assert!(error.contains("use ctx.fs.patch for several"), "{error}");
}

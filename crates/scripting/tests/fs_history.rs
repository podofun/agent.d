use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};
use serde_json::{Value, json};

fn host(root: &std::path::Path, script: &str, history: agentd_fs::History) -> LuaHost {
    let source = root.join("tool.lua");
    std::fs::write(&source, script).unwrap();
    let host = LuaHost::new().unwrap();
    host.set_workspace_root(root);
    host.set_file_history(history);
    host.load_file(&source).unwrap();
    host
}

async fn call(host: &LuaHost, grants: &[&str], args: Value) -> Result<Value, String> {
    host.call(
        CallContext {
            caller: Caller::interface("test"),
            effective_grants: PermissionSet::from_iter(grants.iter().copied()),
            call_chain: Vec::new(),
            cwd: None,
        },
        ActionCall {
            action: "test".into(),
            args,
        },
    )
    .await
    .map(|r| r.value)
    .map_err(|e| e.to_string())
}

#[tokio::test]
async fn lua_binary_history_and_branch_restore() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(
        dir.path(),
        r#"
        agentd.action("test", function(_, ctx)
            local fs = ctx.fs
            local path = "bytes.bin"
            local first = fs.write(path, string.char(0, 255, 128))
            local second = fs.append(path, string.char(254))
            assert(fs.read(path) == string.char(0, 255, 128, 254))
            local diff = fs.diff(path, second)
            assert(diff.revision.offset == 3)
            assert(diff.removed == "" and diff.added == string.char(254))
            assert(fs.undo(path) == first)
            assert(fs.redo(path) == second)
            fs.undo(path)
            local branch = fs.write(path, string.char(129, 0))
            fs.restore(path, second)
            assert(fs.read(path) == string.char(0, 255, 128, 254))
            fs.restore(path, branch)
            assert(fs.read(path) == string.char(129, 0))
            fs.remove(path)
            assert(not fs.exists(path))
            fs.undo(path)
            assert(fs.read(path) == string.char(129, 0))
            fs.restore(path, 0)
            assert(not fs.exists(path))
            return fs.history(path)
        end)
    "#,
        agentd_fs::History::default(),
    );
    let result = call(
        &host,
        &["fs.read:bytes.bin", "fs.write:bytes.bin"],
        Value::Null,
    )
    .await
    .unwrap();
    assert_eq!(result["current"], 0);
    assert_eq!(result["revisions"].as_array().unwrap().len(), 4);
}

const DISPATCH: &str = r#"
    agentd.action("test", function(args, ctx)
        return ctx.fs[args.method](args.path or "data.bin", args.value)
    end)
"#;

#[tokio::test]
async fn history_and_replay_enforce_read_and_write_grants() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(dir.path(), DISPATCH, agentd_fs::History::default());
    call(
        &host,
        &["fs.write:data.bin"],
        json!({"method":"write", "value":"one"}),
    )
    .await
    .unwrap();
    call(
        &host,
        &["fs.write:data.bin"],
        json!({"method":"write", "value":"two"}),
    )
    .await
    .unwrap();
    for method in ["history", "diff"] {
        let error = call(
            &host,
            &["fs.write:data.bin"],
            json!({"method":method, "value":1}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("fs.read"), "{error}");
    }
    for method in ["undo", "redo", "restore", "remove"] {
        let error = call(
            &host,
            &["fs.read:data.bin"],
            json!({"method":method, "value":0}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("fs.write"), "{error}");
    }
    assert_eq!(std::fs::read(dir.path().join("data.bin")).unwrap(), b"two");
    call(&host, &["fs.write:data.bin"], json!({"method":"undo"}))
        .await
        .unwrap();
    call(&host, &["fs.write:data.bin"], json!({"method":"redo"}))
        .await
        .unwrap();
    call(
        &host,
        &["fs.write:data.bin"],
        json!({"method":"restore", "value":1}),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(dir.path().join("data.bin")).unwrap(), b"one");
    let log = call(&host, &["fs.read:data.bin"], json!({"method":"history"}))
        .await
        .unwrap();
    assert_eq!(log["current"], 1);
    assert_eq!(log["revisions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn shared_history_survives_host_rebuild_and_uses_resolved_paths() {
    let dir = tempfile::tempdir().unwrap();
    let history = agentd_fs::History::default();
    {
        let host = host(dir.path(), DISPATCH, history.clone());
        call(
            &host,
            &["fs.write:**"],
            json!({"method":"write", "path":"nested/data.bin", "value":"one"}),
        )
        .await
        .unwrap();
    }
    let host = host(dir.path(), DISPATCH, history);
    call(
        &host,
        &["fs.write:**"],
        json!({"method":"undo", "path":"nested/../nested/data.bin"}),
    )
    .await
    .unwrap();
    assert!(!dir.path().join("nested").exists());
    call(
        &host,
        &["fs.write:**"],
        json!({"method":"redo", "path":"nested/data.bin"}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("nested/data.bin")).unwrap(),
        b"one"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn missing_descendant_of_symlink_cannot_escape_grants() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let host = host(dir.path(), DISPATCH, agentd_fs::History::default());
    let grant = format!("fs.write:{}/**", dir.path().display());
    for path in ["link/missing/data.bin", "missing/../link/missing/data.bin"] {
        let error = call(
            &host,
            &[&grant],
            json!({"method":"write", "path":path, "value":"denied"}),
        )
        .await
        .unwrap_err();
        assert!(error.contains("fs.write"), "{error}");
    }
    assert!(!outside.path().join("missing").exists());
}

async fn call_as(
    host: &LuaHost,
    caller: Caller,
    grants: &[&str],
    args: Value,
) -> Result<Value, String> {
    host.call(
        CallContext {
            caller,
            effective_grants: PermissionSet::from_iter(grants.iter().copied()),
            call_chain: Vec::new(),
            cwd: None,
        },
        ActionCall {
            action: "test".into(),
            args,
        },
    )
    .await
    .map(|r| r.value)
    .map_err(|e| e.to_string())
}

fn in_session(id: &str) -> Caller {
    let mut caller = Caller::interface("test");
    caller.session_id = Some(id.into());
    caller
}

const EDIT: &str = r#"
agentd.action("test", function(args, ctx)
  if args.read then ctx.fs.read(args.path) end
  return ctx.fs.edit(args.path, args.old, args.new, { replace_all = args.all })
end)
"#;

#[tokio::test]
async fn lua_edit_needs_a_read_in_the_same_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "hello world").unwrap();
    let host = host(&root, EDIT, agentd_fs::History::default());
    let grants = ["fs.read:a.txt", "fs.write:a.txt"];

    let error = call_as(
        &host,
        in_session("s1"),
        &grants,
        json!({ "path": "a.txt", "old": "world", "new": "there" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("read `"), "{error}");

    call_as(
        &host,
        in_session("s1"),
        &grants,
        json!({ "path": "a.txt", "read": true, "old": "world", "new": "there" }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "hello there"
    );

    // A read in one call counts for the next call in the same session.
    call_as(
        &host,
        in_session("s1"),
        &grants,
        json!({ "path": "a.txt", "old": "hello", "new": "hi" }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "hi there"
    );

    let error = call_as(
        &host,
        in_session("s2"),
        &grants,
        json!({ "path": "a.txt", "old": "hi", "new": "yo" }),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("read `"),
        "another session has not read it: {error}"
    );
}

#[tokio::test]
async fn lua_edit_is_refused_without_a_write_grant() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    let host = host(&root, EDIT, agentd_fs::History::default());
    let error = call_as(
        &host,
        in_session("s1"),
        &["fs.read:a.txt"],
        json!({ "path": "a.txt", "read": true, "old": "x", "new": "y" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("fs.write"), "{error}");
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "x");
}

#[tokio::test]
async fn edit_without_any_scope_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    let host = host(&root, EDIT, agentd_fs::History::default());
    let error = call_as(
        &host,
        Caller::interface("test"),
        &["fs.read:a.txt", "fs.write:a.txt"],
        json!({ "path": "a.txt", "read": true, "old": "x", "new": "y" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("needs a chat session or a run"), "{error}");
}

const PATCH: &str = r#"
agentd.action("test", function(args, ctx)
  for _, path in ipairs(args.read or {}) do ctx.fs.read(path) end
  return ctx.fs.patch(args.patch)
end)
"#;

#[tokio::test]
async fn lua_patch_respects_grants_and_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "1\n").unwrap();
    std::fs::write(root.join("b.txt"), "x\n").unwrap();
    let host = host(&root, PATCH, agentd_fs::History::default());
    let both = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-1\n+2\n--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n";

    let error = call_as(
        &host,
        in_session("s1"),
        &["fs.read:a.txt", "fs.read:b.txt", "fs.write:a.txt"],
        json!({ "read": ["a.txt", "b.txt"], "patch": both }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("b.txt"), "{error}");
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "1\n");

    let escape = "--- a/../escape.txt\n+++ b/../escape.txt\n@@ -0,0 +1 @@\n+x\n";
    let error = call_as(
        &host,
        in_session("s1"),
        &["fs.read:a.txt", "fs.write:a.txt"],
        json!({ "patch": escape.replace("--- a/../escape.txt", "--- /dev/null") }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("fs.write"), "{error}");
    assert!(!root.parent().unwrap().join("escape.txt").exists());

    let value = call_as(
        &host,
        in_session("s1"),
        &[
            "fs.read:a.txt",
            "fs.read:b.txt",
            "fs.write:a.txt",
            "fs.write:b.txt",
        ],
        json!({ "read": ["a.txt", "b.txt"], "patch": both }),
    )
    .await
    .unwrap();
    assert_eq!(value[0]["path"], "a.txt");
    assert!(value[1]["revision"].as_u64().unwrap() >= 1);
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "2\n");
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "y\n");
}

#[cfg(unix)]
#[tokio::test]
async fn reading_a_hard_linked_file_still_works_in_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "shared").unwrap();
    std::fs::hard_link(root.join("a.txt"), root.join("b.txt")).unwrap();
    let host = host(&root, READ, agentd_fs::History::default());
    let value = call_as(
        &host,
        in_session("s1"),
        &["fs.read:a.txt"],
        json!({ "path": "a.txt" }),
    )
    .await
    .unwrap();
    assert_eq!(value, "shared");
}

const READ: &str = r#"
agentd.action("test", function(args, ctx) return ctx.fs.read(args.path) end)
"#;

#[tokio::test]
async fn a_write_counts_as_a_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let host = host(
        &root,
        r#"
        agentd.action("test", function(args, ctx)
          ctx.fs.write("new.txt", "hello world")
          return ctx.fs.edit("new.txt", "world", "there")
        end)
        "#,
        agentd_fs::History::default(),
    );
    call_as(
        &host,
        in_session("s1"),
        &["fs.read:new.txt", "fs.write:new.txt"],
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt")).unwrap(),
        "hello there"
    );
}

#[tokio::test]
async fn a_missing_scope_is_reported_before_any_permission_check() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("a.txt"), "x").unwrap();
    let host = host(&root, EDIT, agentd_fs::History::default());
    let error = call_as(
        &host,
        Caller::interface("test"),
        &[],
        json!({ "path": "a.txt", "old": "x", "new": "y" }),
    )
    .await
    .unwrap_err();
    assert!(error.contains("needs a chat session or a run"), "{error}");
}

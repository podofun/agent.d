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

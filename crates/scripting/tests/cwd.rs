//! Per-execution cwd + portable relative grants.
//!
//! Covers:
//! - default cwd = workspace root (relative `fs.read` + relative grant)
//! - component `cwd = "sub"` rebases relative paths for that action
//! - a relative grant resolves against the effective cwd (portable grants)
//! - `ctx.fs.chdir` rebases subsequent reads
//! - `ctx.fs.with_cwd` restores the previous cwd on the way out

use std::io::Write;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{ActionCall, CallContext, Registry};

fn ctx(grants: &[&str]) -> CallContext {
    CallContext {
        caller: Caller::interface("test"),
        effective_grants: PermissionSet::from_iter(grants.iter().copied()),
        call_chain: vec!["t.run".to_string()],
        cwd: None,
    }
}

/// Build a workspace dir with the given files (path relative to root → contents)
/// and a single tool script, then a host rooted at that workspace.
fn host_with(files: &[(&str, &str)], tool_src: &str) -> (tempfile::TempDir, LuaHost) {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        let p = dir.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }
    let tool = dir.path().join("tool.lua");
    std::fs::File::create(&tool)
        .unwrap()
        .write_all(tool_src.as_bytes())
        .unwrap();

    let host = LuaHost::new().unwrap();
    host.set_workspace_root(dir.path());
    host.load_file(&tool).unwrap();
    (dir, host)
}

async fn call(host: &LuaHost, action: &str, grants: &[&str]) -> Result<serde_json::Value, String> {
    host.call(
        ctx(grants),
        ActionCall {
            action: action.into(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .map(|r| r.value)
    .map_err(|e| e.to_string())
}

#[tokio::test]
async fn default_cwd_is_workspace_root() {
    let (_d, host) = host_with(
        &[("root.txt", "hi-root")],
        r#"
        agentd.action{
          name = "t.read",
          requires = { "fs.read:root.txt" },
          handler = function(_, ctx) return { body = ctx.fs.read("root.txt") } end,
        }
        "#,
    );
    // Relative grant `fs.read:root.txt` resolves against the workspace root, the
    // same base the relative read resolves against — so this is allowed.
    let v = call(&host, "t.read", &["fs.read:root.txt"]).await.unwrap();
    assert_eq!(v["body"], "hi-root");
}

#[tokio::test]
async fn component_cwd_rebases_relative_paths() {
    let (_d, host) = host_with(
        &[("sub/data.txt", "hi-sub")],
        r#"
        agentd.action{
          name = "t.read",
          cwd = "sub",
          requires = { "fs.read:data.txt" },
          handler = function(_, ctx) return { body = ctx.fs.read("data.txt") } end,
        }
        "#,
    );
    // `cwd = "sub"` + relative grant `fs.read:data.txt` both anchor to <root>/sub.
    let v = call(&host, "t.read", &["fs.read:data.txt"]).await.unwrap();
    assert_eq!(v["body"], "hi-sub");
}

#[tokio::test]
async fn relative_read_outside_grant_is_denied() {
    let (_d, host) = host_with(
        &[("secret.txt", "nope")],
        r#"
        agentd.action{
          name = "t.read",
          handler = function(_, ctx) return { body = ctx.fs.read("secret.txt") } end,
        }
        "#,
    );
    // No grant at all → denied even though the path is relative to the workspace.
    let err = call(&host, "t.read", &[]).await.unwrap_err();
    assert!(err.contains("fs.read"), "got: {err}");
}

#[tokio::test]
async fn chdir_rebases_subsequent_reads() {
    let (_d, host) = host_with(
        &[("sub/data.txt", "hi-chdir")],
        r#"
        agentd.action{
          name = "t.read",
          requires = { "fs.read:sub/data.txt" },
          handler = function(_, ctx)
            ctx.fs.chdir("sub")
            return { body = ctx.fs.read("data.txt"), cwd = ctx.fs.getcwd() }
          end,
        }
        "#,
    );
    // Grant anchors to the workspace root (<root>/sub/data.txt); after chdir the
    // relative read resolves to the same absolute path.
    let v = call(&host, "t.read", &["fs.read:sub/data.txt"])
        .await
        .unwrap();
    assert_eq!(v["body"], "hi-chdir");
}

#[tokio::test]
async fn with_cwd_restores_previous_cwd() {
    let (_d, host) = host_with(
        &[("sub/data.txt", "hi-scoped")],
        r#"
        agentd.action{
          name = "t.read",
          requires = { "fs.read:sub/data.txt" },
          handler = function(_, ctx)
            local before = ctx.fs.getcwd()
            local body = ctx.fs.with_cwd("sub", function()
              return ctx.fs.read("data.txt")
            end)
            local after = ctx.fs.getcwd()
            return { body = body, restored = (before == after) }
          end,
        }
        "#,
    );
    let v = call(&host, "t.read", &["fs.read:sub/data.txt"])
        .await
        .unwrap();
    assert_eq!(v["body"], "hi-scoped");
    assert_eq!(v["restored"], true);
}

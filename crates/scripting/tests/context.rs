//! context.* (sandbox boundary) integration tests.
//!
//! Covers:
//! - sandbox: `io`, `os`, `require` are nil in user code
//! - context.log.info: callable without permissions
//! - context.shell.exec: allowed iff effective_grants covers shell.exec
//! - context.tools.list / .call: recursive call respects effective_grants
//! - empty effective_grants blocks everything except log

use std::io::Write;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
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

fn ctx(grants: &[&str], chain: &[&str]) -> CallContext {
    CallContext {
        caller: Caller::interface("test"),
        effective_grants: PermissionSet::from_iter(grants.iter().copied()),
        call_chain: chain.iter().map(|s| s.to_string()).collect(),
    }
}

#[tokio::test]
async fn sandbox_blocks_io_in_user_code() {
    let dir = write_tools(&[(
        "bad.lua",
        r#"
            agentd.action{
              name = "bad.touch",
              handler = function(_, ctx)
                -- io is nil; this should error
                local f = io.open("/etc/passwd", "r")
                return { ok = true }
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[], &["bad.touch"]),
            ActionCall {
                action: "bad.touch".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("io") || msg.contains("nil"), "got: {msg}");
}

#[tokio::test]
async fn log_works_without_permissions() {
    let dir = write_tools(&[(
        "loud.lua",
        r#"
            agentd.action{
              name = "loud.say",
              handler = function(_, ctx)
                ctx.log.info("hello from lua")
                return { said = true }
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&[], &["loud.say"]),
            ActionCall {
                action: "loud.say".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value, serde_json::json!({ "said": true }));
}

#[tokio::test]
async fn shell_exec_denied_without_grant() {
    let dir = write_tools(&[(
        "s.lua",
        r#"
            agentd.action{
              name = "s.echo",
              handler = function(_, ctx)
                return ctx.shell("echo", {"hi"})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[], &["s.echo"]),
            ActionCall {
                action: "s.echo".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("shell.exec"), "got: {msg}");
}

#[tokio::test]
async fn shell_exec_allowed_with_grant() {
    let dir = write_tools(&[(
        "s.lua",
        r#"
            agentd.action{
              name = "s.echo",
              handler = function(_, ctx)
                return ctx.shell("/bin/echo", {"hi"})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["shell.exec"], &["s.echo"]),
            ActionCall {
                action: "s.echo".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let stdout = res.value.get("stdout").and_then(|v| v.as_str()).unwrap();
    assert_eq!(stdout.trim(), "hi");
    let code = res.value.get("exit_code").and_then(|v| v.as_i64()).unwrap();
    assert_eq!(code, 0);
}

#[tokio::test]
async fn shell_exec_scoped_allows_matching_bin() {
    // A scoped `shell.exec:<bin>` grant authorizes exactly that binary.
    let dir = write_tools(&[(
        "s.lua",
        r#"
            agentd.action{
              name = "s.echo",
              handler = function(_, ctx)
                return ctx.shell("/bin/echo", {"hi"})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["shell.exec:/bin/echo"], &["s.echo"]),
            ActionCall {
                action: "s.echo".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let stdout = res.value.get("stdout").and_then(|v| v.as_str()).unwrap();
    assert_eq!(stdout.trim(), "hi");
}

#[tokio::test]
async fn shell_exec_scoped_denies_other_bin() {
    // Granting only `shell.exec:/bin/echo` must NOT authorize a different binary.
    let dir = write_tools(&[(
        "s.lua",
        r#"
            agentd.action{
              name = "s.cat",
              handler = function(_, ctx)
                return ctx.shell("/bin/cat", {"/etc/hostname"})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&["shell.exec:/bin/echo"], &["s.cat"]),
            ActionCall {
                action: "s.cat".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("shell.exec:/bin/cat"),
        "expected denial naming the unscoped binary, got: {msg}"
    );
}

#[tokio::test]
async fn tools_list_includes_registered() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
            agentd.action{
              name = "t.first",
              handler = function(_, ctx) return {} end,
            }
            agentd.action{
              name = "t.second",
              handler = function(_, ctx) return {} end,
            }
            agentd.action{
              name = "t.who",
              handler = function(_, ctx)
                return { tools = ctx.tools() }
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&[], &["t.who"]),
            ActionCall {
                action: "t.who".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let tools = res.value.get("tools").and_then(|v| v.as_array()).unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|v| v.as_str()).collect();
    assert!(names.contains(&"t.first"));
    assert!(names.contains(&"t.second"));
    assert!(names.contains(&"t.who"));
}

#[tokio::test]
async fn tools_call_denied_when_inner_requires_unmet() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
            agentd.action{
              name = "inner.danger",
              requires = { "shell.exec" },
              handler = function(_, ctx) return { ok = true } end,
            }
            agentd.action{
              name = "outer.call",
              handler = function(_, ctx)
                return ctx.call("inner.danger", {})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    // Outer has no grants; tools.call must deny.
    let err = host
        .call(
            ctx(&[], &["outer.call"]),
            ActionCall {
                action: "outer.call".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("shell.exec"), "got: {err}");
}

#[tokio::test]
async fn tools_call_allowed_when_grants_cover_inner() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
            agentd.action{
              name = "inner.echo",
              requires = { "shell.exec" },
              handler = function(_, ctx)
                return ctx.shell("/bin/echo", {"ok"})
              end,
            }
            agentd.action{
              name = "outer.wrap",
              handler = function(_, ctx)
                return ctx.call("inner.echo", {})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let res = host
        .call(
            ctx(&["shell.exec"], &["outer.wrap"]),
            ActionCall {
                action: "outer.wrap".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let stdout = res.value.get("stdout").and_then(|v| v.as_str()).unwrap();
    assert_eq!(stdout.trim(), "ok");
}

#[tokio::test]
async fn tools_call_blocks_confirm_actions() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
            agentd.action{
              name = "danger.go",
              requires = {},
              confirm = true,
              handler = function(_, ctx) return { boom = true } end,
            }
            agentd.action{
              name = "wrap.go",
              handler = function(_, ctx)
                return ctx.call("danger.go", {})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let err = host
        .call(
            ctx(&[], &["wrap.go"]),
            ActionCall {
                action: "wrap.go".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("confirmation"), "got: {err}");
}

/// End-to-end: the OS sandbox confines a ctx.shell child to its fs.write grant.
/// Skipped when the kernel has no enforcing Landlock backend.
#[tokio::test]
async fn shell_child_confined_to_fs_write_grant() {
    if !agentd_shell::sandbox::is_supported() {
        eprintln!("native sandbox unsupported; skipping");
        return;
    }
    let granted = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let inside_path = granted.path().join("ok.txt");
    let outside_path = outside.path().join("nope.txt");

    let dir = write_tools(&[(
        "w.lua",
        r#"
            agentd.action{
              name = "w.write",
              handler = function(args, ctx)
                return ctx.shell("/bin/sh", {"-c", "echo hi > " .. args.path})
              end,
            }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();

    let write_grant = format!("fs.write:{}/**", granted.path().display());
    let grants = PermissionSet::from_iter(["shell.exec:/bin/sh", write_grant.as_str()]);
    let make_ctx = || CallContext {
        caller: Caller::interface("test"),
        effective_grants: grants.clone(),
        call_chain: vec!["w.write".to_string()],
    };

    // Inside the grant: write succeeds.
    let res = host
        .call(
            make_ctx(),
            ActionCall {
                action: "w.write".into(),
                args: serde_json::json!({ "path": inside_path.display().to_string() }),
            },
        )
        .await
        .unwrap();
    assert_eq!(res.value["exit_code"], 0, "inside-grant write should succeed");
    assert!(inside_path.exists(), "file inside grant must exist");

    // Outside the grant: Landlock denies the write, sh exits nonzero.
    let res = host
        .call(
            make_ctx(),
            ActionCall {
                action: "w.write".into(),
                args: serde_json::json!({ "path": outside_path.display().to_string() }),
            },
        )
        .await
        .unwrap();
    assert_ne!(res.value["exit_code"], 0, "outside-grant write must be denied");
    assert!(!outside_path.exists(), "file outside grant must NOT exist");
}

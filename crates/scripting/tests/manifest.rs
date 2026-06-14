//! Lua manifest API.
//!
//! Covers:
//! - positional `agentd.action("name", fn)` (no perms inferred)
//! - table form `agentd.action{ name, requires, confirm, handler }`
//! - `agentd.tool{ name, requires }`
//! - Registry::action_info / tool_info expose the metadata
//! - tool inferred from action name when not set explicitly

use std::io::Write;

use agentd_scripting::LuaHost;
use agentd_types::Registry;

fn write_tmp(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("tool.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

#[test]
fn positional_register_has_no_requires() {
    let dir = write_tmp(
        r#"
        agentd.action("ping", function(_, ctx) return { ok = true } end)
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let info = host.action_info("ping").expect("action registered");
    assert!(info.requires.is_empty());
    assert_eq!(info.tool, None);
    assert!(!info.confirm);
}

#[test]
fn table_register_captures_requires_and_tool_inference() {
    let dir = write_tmp(
        r#"
        agentd.action{
          name = "git.diff",
          requires = { "shell.exec" },
          handler = function(_, ctx) return {} end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let info = host.action_info("git.diff").unwrap();
    assert_eq!(info.tool.as_deref(), Some("git"));
    assert_eq!(info.requires, vec!["shell.exec".to_string()]);
}

#[test]
fn confirm_flag_captured() {
    let dir = write_tmp(
        r#"
        agentd.action{
          name = "danger.drop",
          requires = { "fs.write:/**" },
          confirm = true,
          handler = function(_, ctx) return {} end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let info = host.action_info("danger.drop").unwrap();
    assert!(info.confirm);
}

#[test]
fn tool_manifest_registered() {
    let dir = write_tmp(
        r#"
        agentd.tool{
          name = "google_calendar",
          requires = { "net:googleapis.com", "oauth:google" },
        }
        agentd.action{
          name = "google_calendar.list",
          requires = { "calendar.read" },
          tool = "google_calendar",
          handler = function(_, ctx) return {} end,
        }
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let tool = host.tool_info("google_calendar").unwrap();
    assert!(tool.requires.iter().any(|s| s == "oauth:google"));
    let action = host.action_info("google_calendar.list").unwrap();
    assert_eq!(action.tool.as_deref(), Some("google_calendar"));
    assert_eq!(action.requires, vec!["calendar.read".to_string()]);
}

#[test]
fn missing_handler_in_table_form_fails() {
    let dir = write_tmp(
        r#"
        agentd.action{ name = "broken", requires = {} }
        "#,
    );
    let host = LuaHost::new().unwrap();
    let err = host.load_dir(dir.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("handler"), "got: {msg}");
}

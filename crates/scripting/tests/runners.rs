//! Lua `agentd.runner{...}` registration writes into the shared
//! `RunnerRegistry` exposed by `LuaHost::runners()`.

use std::io::Write;

use agentd_scripting::LuaHost;

fn write_tmp(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("runner.lua");
    let mut f = std::fs::File::create(&p).unwrap();
    f.write_all(body.as_bytes()).unwrap();
    dir
}

#[test]
fn runner_registration_round_trips() {
    let dir = write_tmp(
        r#"
        agentd.runner({
            name = "backend_reviewer",
            system = "Be terse.",
            model = "anthropic/claude-opus-4-7",
            skills = { "reviewer", "debugger" },
            actions = { "git.diff", "github.comment_pr" },
        })
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let def = host
        .runners()
        .get("backend_reviewer")
        .expect("runner registered");
    assert_eq!(def.system.as_deref(), Some("Be terse."));
    assert_eq!(def.model.as_deref(), Some("anthropic/claude-opus-4-7"));
    assert_eq!(def.skills, vec!["reviewer", "debugger"]);
    assert_eq!(def.allowed_actions, vec!["git.diff", "github.comment_pr"]);
}

#[test]
fn legacy_provider_field_rejected() {
    let dir = write_tmp(
        r#"
        agentd.runner({
            name = "r",
            provider = "claude-cli",
            model = "claude-opus-4-7",
        })
        "#,
    );
    let host = LuaHost::new().unwrap();
    let err = host.load_dir(dir.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("provider") && msg.contains("<provider>/<model>"),
        "expected provider-removed diagnostic: {msg}"
    );
}

#[test]
fn allowed_actions_alias_takes_precedence_over_actions() {
    let dir = write_tmp(
        r#"
        agentd.runner({
            name = "r",
            allowed_actions = { "a" },
            actions = { "b" },
        })
        "#,
    );
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let def = host.runners().get("r").unwrap();
    assert_eq!(def.allowed_actions, vec!["a"]);
}

#[test]
fn name_is_required() {
    let dir = write_tmp(
        r#"
        agentd.runner({ system = "no name" })
        "#,
    );
    let host = LuaHost::new().unwrap();
    let err = host.load_dir(dir.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("name"),
        "expected error mentioning `name`: {msg}"
    );
}

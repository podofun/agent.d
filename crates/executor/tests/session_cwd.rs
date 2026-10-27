//! Runs in a session with a working folder work in that folder, and
//! `{cwd}` grants cover it and nothing else.

use std::sync::Arc;

use agentd_ai::{MockProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{Caller, Engine, Grants};
use agentd_runners::RunOptions;
use agentd_scripting::LuaHost;
use agentd_sessions::{MemSessionStore, NewSession, SessionStore};
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::Registry;
use async_trait::async_trait;

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

const LUA: &str = r#"
agentd.tool{ name = "note" }
agentd.action{
  name = "note.write",
  handler = function(args, ctx)
    ctx.fs.write(args.path, "x")
    return ctx.fs.getcwd()
  end,
}
agentd.runner{ name = "chat", model = "mock/m", actions = { "note.write" } }
agentd.runner{ name = "pinned", model = "mock/m", actions = { "note.write" }, cwd = "PINNED" }
"#;

const GRANTS: &str = r#"
[tool.note]
granted = ["fs.write:{cwd}/**", "fs.write:PINNED/**"]

[runner.chat]
granted = ["ai:mock"]
allowed_actions = ["note.write"]

[runner.pinned]
granted = ["ai:mock"]
allowed_actions = ["note.write"]
"#;

struct Setup {
    exec: Arc<Executor>,
    store: Arc<MemSessionStore>,
    folder: std::path::PathBuf,
    pinned: std::path::PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

fn setup(path: &str) -> Setup {
    let config = tempfile::tempdir().unwrap();
    let folder_dir = tempfile::tempdir().unwrap();
    let pinned_dir = tempfile::tempdir().unwrap();
    let folder = folder_dir.path().canonicalize().unwrap();
    let pinned = pinned_dir.path().canonicalize().unwrap();
    let lua = LUA.replace("PINNED", &pinned.to_string_lossy());
    let grants = GRANTS.replace("PINNED", &pinned.to_string_lossy());
    std::fs::write(config.path().join("init.lua"), lua).unwrap();
    let host = LuaHost::new().unwrap();
    host.set_root(config.path());
    host.set_workspace_root(config.path());
    host.load_file(&config.path().join("init.lua")).unwrap();
    let (skills, runners, services) = (host.skills(), host.runners(), host.services());
    let registry: Arc<dyn Registry> = Arc::new(host);
    let provider = MockProvider::new().with_script(vec![
        MockProvider::tool_call("c1", "note.write", serde_json::json!({ "path": path })),
        MockProvider::text_only("done"),
    ]);
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(provider));
    providers.set_default("mock");
    let file = agentd_permissions::parse_grants(&grants).unwrap();
    let mut exec = Executor::new(
        registry,
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        services,
        skills,
        Arc::new(providers),
    );
    let store = Arc::new(MemSessionStore::new());
    exec.set_sessions(store.clone());
    Setup {
        exec: Arc::new(exec),
        store,
        folder,
        pinned,
        _dirs: vec![config, folder_dir, pinned_dir],
    }
}

async fn run(s: &Setup, runner: &str) -> Vec<agentd_ai::Message> {
    let session = s
        .store
        .create(
            NewSession::in_scope(&agentd_executor::scope_of(&caller()))
                .cwd(Some(s.folder.to_string_lossy().into_owned())),
        )
        .unwrap();
    let options = RunOptions {
        prompt: Some("write it".into()),
        session_id: Some(session.id.clone()),
        ..Default::default()
    };
    s.exec
        .run_runner_with_options(caller(), runner, options, None)
        .await
        .unwrap();
    s.store.turns(&session.id).unwrap()
}

fn caller() -> Caller {
    Caller::interface("ws")
}

fn tool_output(turns: &[agentd_ai::Message]) -> String {
    turns
        .iter()
        .find(|m| m.tool_call_id.is_some())
        .map(|m| m.content.clone())
        .unwrap()
}

#[tokio::test]
async fn session_runs_work_in_the_session_folder() {
    let s = setup("note.txt");
    let turns = run(&s, "chat").await;
    assert!(s.folder.join("note.txt").exists());
    assert!(
        tool_output(&turns).contains(&*s.folder.to_string_lossy()),
        "{turns:?}"
    );
}

#[tokio::test]
async fn writing_outside_the_session_folder_is_refused() {
    let s = setup("../outside.txt");
    let turns = run(&s, "chat").await;
    assert!(!s.folder.parent().unwrap().join("outside.txt").exists());
    assert!(tool_output(&turns).contains("fs.write"), "{turns:?}");
}

#[tokio::test]
async fn a_runner_with_its_own_cwd_still_wins() {
    let s = setup("note.txt");
    run(&s, "pinned").await;
    assert!(s.pinned.join("note.txt").exists());
    assert!(!s.folder.join("note.txt").exists());
}

#[tokio::test]
async fn a_nested_run_without_a_session_keeps_its_parents_folder() {
    let s = setup("note.txt");
    // A run started by another run (ctx.run) carries its parent's caller.
    let mut parent = caller();
    parent.cwd = Some(s.folder.to_string_lossy().into_owned());
    let options = RunOptions {
        prompt: Some("write it".into()),
        ..Default::default()
    };
    s.exec
        .run_runner_with_options(parent, "chat", options, None)
        .await
        .unwrap();
    assert!(s.folder.join("note.txt").exists());
}

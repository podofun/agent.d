//! Every file an action call changes is recorded in file history, undoable,
//! and reported, however the call changed it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentd_ai::{ChangeKind, FileChange, MockProvider, ProviderRegistry};
use agentd_executor::{Executor, ExecutorHandle};
use agentd_permissions::{Caller, Engine, Grants};
use agentd_scripting::LuaHost;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{ActionCall, Registry};
use async_trait::async_trait;
use serde_json::json;

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

const LUA: &str = r#"
local d = agentd

d.tool({ name = "files" })
d.tool({ name = "plain" })

d.action({
  name = "files.write",
  tool = "files",
  handler = function(args, ctx)
    return ctx.fs.write(args.path, args.content)
  end,
})

d.action({
  name = "files.sh",
  tool = "files",
  handler = function(args, ctx)
    return ctx.shell("sh", { "-c", args.script })
  end,
})

d.action({
  name = "files.write_then_sh",
  tool = "files",
  handler = function(args, ctx)
    ctx.fs.write(args.path, args.content)
    return ctx.shell("sh", { "-c", args.script })
  end,
})

d.action({
  name = "files.delegate",
  tool = "files",
  handler = function(_, ctx)
    ctx.fs.write("outer.txt", "outer")
    return ctx.run("helper", { prompt = "write it" })
  end,
})

d.action({
  name = "plain.noop",
  tool = "plain",
  handler = function()
    return true
  end,
})

d.runner({ name = "helper", model = "mock/m", actions = { "files.write" } })
"#;

const GRANTS: &str = r#"
[tool.files]
granted = ["fs.read:ROOT/**", "fs.write:ROOT/**", "shell.exec:sh", "runner:helper"]

[tool.plain]
granted = ["fs.read:ROOT/**", "shell.exec:sh"]

[runner.helper]
granted = ["ai:mock"]
allowed_actions = ["files.write"]
"#;

struct Setup {
    exec: Arc<Executor>,
    history: agentd_fs::History,
    baseline: agentd_fs::Baseline,
    root: PathBuf,
    project: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

fn setup() -> Setup {
    setup_with(MockProvider::new().with_reply("done"))
}

fn setup_with(provider: MockProvider) -> Setup {
    let config = tempfile::tempdir().unwrap();
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(config.path().join("init.lua"), LUA).unwrap();
    let history = agentd_fs::History::default();
    let baseline = agentd_fs::Baseline::default();
    let host = Arc::new(LuaHost::new().unwrap());
    host.set_root(config.path());
    host.set_workspace_root(&project);
    host.set_file_history(history.clone());
    host.load_file(&config.path().join("init.lua")).unwrap();
    let (skills, runners, services) = (host.skills(), host.runners(), host.services());
    let registry: Arc<dyn Registry> = host.clone();
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(provider));
    providers.set_default("mock");
    let grants = GRANTS.replace("ROOT", &root.to_string_lossy());
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
    exec.set_workspace_root(project.clone());
    exec.set_file_capture(history.clone(), baseline.clone());
    let exec = Arc::new(exec);
    host.set_runner_dispatcher(ExecutorHandle::new(exec.clone()));
    Setup {
        exec,
        history,
        baseline,
        root,
        project,
        _dirs: vec![config, root_dir],
    }
}

fn caller(s: &Setup) -> Caller {
    let mut caller = Caller::interface("test");
    caller.cwd = Some(s.project.to_string_lossy().into_owned());
    caller
}

async fn call(s: &Setup, action: &str, args: serde_json::Value) -> Vec<FileChange> {
    let call = ActionCall {
        action: action.into(),
        args,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        s.exec.run_with_changes(caller(s), call),
    )
    .await
    .expect("the call finished");
    match result {
        Ok((_, _, changes)) => changes,
        Err((error, _)) => panic!("the call failed: {error}"),
    }
}

fn one(changes: &[FileChange], path: &str) -> FileChange {
    let found: Vec<_> = changes.iter().filter(|c| c.path == path).collect();
    assert_eq!(found.len(), 1, "{changes:?}");
    found[0].clone()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

// The tests that change files through a program run `sh`, so they run on unix
// only. `cmd` quoting and line endings differ; the `ctx.fs` tests cover the
// same capture path on Windows.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn shell_changes_are_recorded_and_undoable() {
    let s = setup();
    std::fs::write(s.project.join("a.txt"), "a\n").unwrap();
    let changes = call(&s, "files.sh", json!({ "script": "printf 'b\\n' > a.txt" })).await;
    assert_eq!(changes.len(), 1, "{changes:?}");
    let change = one(&changes, "a.txt");
    assert_eq!(change.operation, ChangeKind::Modified);
    let diff = change.diff.clone().unwrap();
    assert!(diff.contains("-a") && diff.contains("+b"), "{diff}");
    assert_eq!((change.added, change.removed), (1, 1));
    assert!(!change.binary);
    let path = s.project.join("a.txt");
    let log = s.history.history(&path).await.unwrap();
    assert_eq!(change.revision, Some(log.current));
    let actor = log.revisions[0].actor.clone().unwrap();
    assert_eq!(actor.action, "files.sh");
    s.history.undo(&path).await.unwrap();
    assert_eq!(read(&path), "a\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn ctx_fs_writes_are_not_double_recorded() {
    let s = setup();
    let changes = call(
        &s,
        "files.write",
        json!({ "path": "x.txt", "content": "1" }),
    )
    .await;
    let change = one(&changes, "x.txt");
    assert_eq!(change.operation, ChangeKind::Created);
    assert_eq!(change.revision, Some(1));
    let log = s.history.history(&s.project.join("x.txt")).await.unwrap();
    assert_eq!(log.revisions.len(), 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn write_then_formatter_reports_once_and_undoes_in_order() {
    let s = setup();
    let path = s.project.join("f.txt");
    std::fs::write(&path, "original\n").unwrap();
    // Build the baseline first, so the file is known as it was.
    call(&s, "files.sh", json!({ "script": "true" })).await;
    let changes = call(
        &s,
        "files.write_then_sh",
        json!({
            "path": "f.txt",
            "content": "written\n",
            "script": "printf 'formatted\\n' > f.txt",
        }),
    )
    .await;
    let change = one(&changes, "f.txt");
    let diff = change.diff.unwrap();
    assert!(
        diff.contains("-original") && diff.contains("+formatted"),
        "{diff}"
    );
    let log = s.history.history(&path).await.unwrap();
    assert_eq!(log.revisions.len(), 2);
    assert_eq!(change.revision, Some(2));
    s.history.undo(&path).await.unwrap();
    assert_eq!(read(&path), "written\n");
    s.history.undo(&path).await.unwrap();
    assert_eq!(read(&path), "original\n");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn deletions_and_creations_are_reported() {
    let s = setup();
    std::fs::write(s.project.join("a.txt"), "a\n").unwrap();
    call(&s, "files.sh", json!({ "script": "true" })).await;
    let changes = call(
        &s,
        "files.sh",
        json!({ "script": "rm a.txt; echo n > b.txt" }),
    )
    .await;
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert_eq!(one(&changes, "a.txt").operation, ChangeKind::Removed);
    assert_eq!(one(&changes, "b.txt").operation, ChangeKind::Created);
    s.history.undo(&s.project.join("a.txt")).await.unwrap();
    assert_eq!(read(&s.project.join("a.txt")), "a\n");
    s.history.undo(&s.project.join("b.txt")).await.unwrap();
    assert!(!s.project.join("b.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn outside_edits_between_calls_do_not_block_writes() {
    let s = setup();
    let path = s.project.join("a.txt");
    call(
        &s,
        "files.write",
        json!({ "path": "a.txt", "content": "first" }),
    )
    .await;
    std::fs::write(&path, "edited by the user").unwrap();
    let changes = call(
        &s,
        "files.write",
        json!({ "path": "a.txt", "content": "second" }),
    )
    .await;
    let change = one(&changes, "a.txt");
    let diff = change.diff.unwrap();
    assert!(
        diff.contains("-edited by the user") && diff.contains("+second"),
        "{diff}"
    );
    s.history.undo(&path).await.unwrap();
    assert_eq!(read(&path), "edited by the user");
    let error = s.history.undo(&path).await.unwrap_err();
    assert!(
        error.to_string().contains("changed outside agent.d"),
        "{error}"
    );
    assert_eq!(read(&path), "edited by the user");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_write_grant_captures_nothing() {
    let s = setup();
    let changes = call(&s, "plain.noop", json!({})).await;
    assert!(changes.is_empty());
    assert_eq!(s.baseline.scans(), 0);
    call(
        &s,
        "files.write",
        json!({ "path": "a.txt", "content": "a" }),
    )
    .await;
    assert_eq!(s.baseline.scans(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_never_leaves_the_working_folder() {
    let s = setup();
    std::fs::create_dir(s.root.join("other")).unwrap();
    call(
        &s,
        "files.write",
        json!({ "path": "in.txt", "content": "0" }),
    )
    .await;
    let changes = call(
        &s,
        "files.write",
        json!({ "path": "../other/x.txt", "content": "x" }),
    )
    .await;
    assert!(changes.is_empty(), "{changes:?}");
    let changes = call(
        &s,
        "files.write",
        json!({ "path": "in.txt", "content": "1" }),
    )
    .await;
    assert_eq!(changes.len(), 1, "{changes:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn ignored_build_output_is_not_reported() {
    let s = setup();
    std::fs::write(s.project.join(".gitignore"), "out/\n").unwrap();
    call(
        &s,
        "files.write",
        json!({ "path": "keep.txt", "content": "k" }),
    )
    .await;
    let changes = call(
        &s,
        "files.write",
        json!({ "path": "out/x", "content": "x" }),
    )
    .await;
    assert!(changes.is_empty(), "{changes:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_calls_in_one_folder_attribute_changes_once() {
    let s = setup();
    call(
        &s,
        "files.write",
        json!({ "path": "seed.txt", "content": "s" }),
    )
    .await;
    let (a, b) = tokio::join!(
        call(
            &s,
            "files.write",
            json!({ "path": "a.txt", "content": "a" })
        ),
        call(
            &s,
            "files.write",
            json!({ "path": "b.txt", "content": "b" })
        ),
    );
    assert_eq!(a.len(), 1, "{a:?}");
    assert_eq!(one(&a, "a.txt").operation, ChangeKind::Created);
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(one(&b, "b.txt").operation, ChangeKind::Created);
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_calls_in_one_folder_do_not_wait_on_each_other() {
    let s = setup_with(MockProvider::new().with_script(vec![
        MockProvider::tool_call(
            "c1",
            "files.write",
            json!({ "path": "inner.txt", "content": "inner" }),
        ),
        MockProvider::text_only("done"),
    ]));
    call(
        &s,
        "files.write",
        json!({ "path": "seed.txt", "content": "s" }),
    )
    .await;
    let changes = call(&s, "files.delegate", json!({})).await;
    assert_eq!(changes.len(), 2, "{changes:?}");
    one(&changes, "outer.txt");
    one(&changes, "inner.txt");
    let log = s
        .history
        .history(&s.project.join("inner.txt"))
        .await
        .unwrap();
    assert_eq!(log.revisions.len(), 1, "recorded once");
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_results_report_the_files_a_call_changed() {
    let s = setup_with(MockProvider::new().with_script(vec![
        MockProvider::tool_call(
            "c1",
            "files.write",
            json!({ "path": "note.txt", "content": "hello\n" }),
        ),
        MockProvider::text_only("done"),
    ]));
    let (tx, mut rx) = agentd_ai::types::stream_channel();
    s.exec
        .run_runner_with_options(
            caller(&s),
            "helper",
            agentd_runners::RunOptions {
                prompt: Some("write it".into()),
                ..Default::default()
            },
            Some(tx),
        )
        .await
        .unwrap();
    let mut changes = None;
    while let Ok(event) = rx.try_recv() {
        if let agentd_ai::StreamEvent::ToolResult { changes: c, .. } = event {
            changes = Some(c);
        }
    }
    let changes = changes.expect("a tool result");
    let change = one(&changes, "note.txt");
    assert_eq!(change.operation, ChangeKind::Created);
    assert!(change.diff.unwrap().contains("+hello"));
}

//! Every file an action call changes is recorded in file history, undoable,
//! and reported, however the call changed it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd_ai::{ChangeKind, FileChange, MockProvider, ProviderRegistry};
use agentd_executor::{Executor, ExecutorHandle};
use agentd_permissions::{Caller, Engine, Grants};
use agentd_scripting::LuaHost;
use agentd_secrets::SecretStore;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{ActionCall, Registry};
use async_trait::async_trait;
use serde_json::json;

#[derive(Default)]
struct Collect(Mutex<Vec<TraceEvent>>);
#[async_trait]
impl TraceSink for Collect {
    async fn record(&self, e: TraceEvent) {
        self.0.lock().unwrap().push(e);
    }
}

const SECRET: &str = "hunter2-very-secret";

const LUA: &str = r#"
local d = agentd

d.tool({ name = "files" })
d.tool({ name = "plain" })
d.tool({ name = "notes" })

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
  name = "files.read_edit",
  tool = "files",
  handler = function(args, ctx)
    ctx.fs.read(args.path)
    return ctx.fs.edit(args.path, args.old, args.new)
  end,
})

d.action({
  name = "files.remove",
  tool = "files",
  handler = function(args, ctx)
    return ctx.fs.remove(args.path)
  end,
})

d.action({
  name = "files.delegate",
  tool = "files",
  handler = function(args, ctx)
    ctx.fs.write("outer.txt", "outer")
    sleep(args.wait or 0)
    return ctx.run(args.runner or "helper", { prompt = "write it" })
  end,
})

d.action({
  name = "files.slow_write",
  tool = "files",
  handler = function(args, ctx)
    sleep(args.wait)
    return ctx.fs.write(args.path, args.content)
  end,
})

d.action({
  name = "files.history",
  tool = "files",
  handler = function(args, ctx)
    if args.op == "undo" then return ctx.fs.undo(args.path) end
    if args.op == "redo" then return ctx.fs.redo(args.path) end
    return ctx.fs.restore(args.path, args.revision)
  end,
})

d.action({
  name = "files.secret_write",
  tool = "files",
  handler = function(args, ctx)
    return ctx.fs.write(args.path, "token=" .. ctx.secret.get("token") .. "\n")
  end,
})

d.action({
  name = "notes.write",
  tool = "notes",
  handler = function(args, ctx)
    return ctx.fs.write(args.path, args.content)
  end,
})

d.action({
  name = "plain.noop",
  tool = "plain",
  handler = function()
    return true
  end,
})

d.runner({ name = "helper", model = "mock/m", actions = { "files.write", "files.secret_write" } })
d.runner({ name = "helper_x", model = "mock/m", actions = { "files.write" }, cwd = "FOLDER_X" })
d.runner({ name = "helper_y", model = "mock/m", actions = { "files.write" }, cwd = "FOLDER_Y" })
"#;

const GRANTS: &str = r#"
[tool.files]
granted = ["fs.read:ROOT/**", "fs.write:ROOT/**", "shell.exec:sh", "secret:token"]

[tool.plain]
granted = ["fs.read:ROOT/**", "shell.exec:sh"]

[tool.notes]
granted = ["fs.read:ROOT/**", "fs.write:ROOT/**"]

[runner.helper]
granted = ["ai:mock"]
allowed_actions = ["files.write", "files.secret_write"]

[runner.helper_x]
granted = ["ai:mock"]
allowed_actions = ["files.write"]

[runner.helper_y]
granted = ["ai:mock"]
allowed_actions = ["files.write"]
"#;

struct Setup {
    exec: Arc<Executor>,
    sessions: Arc<agentd_sessions::MemSessionStore>,
    history: agentd_fs::History,
    baseline: agentd_fs::Baseline,
    trace: Arc<Collect>,
    project: PathBuf,
    other: PathBuf,
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
    let other = root.join("other");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&other).unwrap();
    let lua = LUA
        .replace("FOLDER_X", &project.to_string_lossy())
        .replace("FOLDER_Y", &other.to_string_lossy());
    std::fs::write(config.path().join("init.lua"), lua).unwrap();
    let history = agentd_fs::History::default();
    let baseline = agentd_fs::Baseline::default();
    let host = Arc::new(LuaHost::new().unwrap());
    host.set_root(config.path());
    host.set_workspace_root(config.path());
    host.set_file_history(history.clone());
    let secrets = agentd_secrets::MemoryStore::new();
    secrets.set("token", SECRET).unwrap();
    host.set_secrets(Arc::new(secrets));
    host.load_file(&config.path().join("init.lua")).unwrap();
    let (skills, runners, services) = (host.skills(), host.runners(), host.services());
    let registry: Arc<dyn Registry> = host.clone();
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(provider));
    providers.set_default("mock");
    let grants = GRANTS.replace("ROOT", &root.to_string_lossy());
    let file = agentd_permissions::parse_grants(&grants).unwrap();
    let trace = Arc::new(Collect::default());
    let mut exec = Executor::new(
        registry,
        trace.clone(),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        services,
        skills,
        Arc::new(providers),
    );
    exec.set_config_folder(config.path().to_path_buf());
    exec.set_file_capture(history.clone(), baseline.clone());
    let sessions = Arc::new(agentd_sessions::MemSessionStore::new());
    exec.set_sessions(sessions.clone());
    let exec = Arc::new(exec);
    host.set_runner_dispatcher(ExecutorHandle::new(exec.clone()));
    host.set_inline_approvals(exec.clone());
    Setup {
        exec,
        sessions,
        history,
        baseline,
        trace,
        project,
        other,
        _dirs: vec![config, root_dir],
    }
}

fn caller_in(folder: &Path) -> Caller {
    let mut caller = Caller::interface("test");
    caller.cwd = Some(folder.to_string_lossy().into_owned());
    caller
}

fn caller(s: &Setup) -> Caller {
    caller_in(&s.project)
}

async fn call_as(
    s: &Setup,
    caller: Caller,
    action: &str,
    args: serde_json::Value,
) -> Vec<FileChange> {
    let call = ActionCall {
        action: action.into(),
        args,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        s.exec.run_with_changes(caller, call),
    )
    .await
    .expect("the call finished");
    match result {
        Ok((_, _, changes)) => changes,
        Err((error, _)) => panic!("the call failed: {error}"),
    }
}

async fn call(s: &Setup, action: &str, args: serde_json::Value) -> Vec<FileChange> {
    call_as(s, caller(s), action, args).await
}

fn one(changes: &[FileChange], path: &str) -> FileChange {
    let found: Vec<_> = changes.iter().filter(|c| c.path == path).collect();
    assert_eq!(found.len(), 1, "{changes:?}");
    found[0].clone()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn undo_redo_and_restore_inside_captured_calls_record_nothing_new() {
    let s = setup();
    let path = s.project.join("a.txt");
    for content in ["1", "2", "3"] {
        call(
            &s,
            "files.write",
            json!({ "path": "a.txt", "content": content }),
        )
        .await;
    }
    let steps = [
        (json!({ "op": "undo", "path": "a.txt" }), "2", 2),
        (json!({ "op": "undo", "path": "a.txt" }), "1", 1),
        (json!({ "op": "redo", "path": "a.txt" }), "2", 2),
        (json!({ "op": "redo", "path": "a.txt" }), "3", 3),
        (
            json!({ "op": "restore", "path": "a.txt", "revision": 1 }),
            "1",
            1,
        ),
        (
            json!({ "op": "restore", "path": "a.txt", "revision": 1 }),
            "1",
            1,
        ),
    ];
    for (args, content, revision) in steps {
        let changes = call(&s, "files.history", args.clone()).await;
        assert_eq!(read(&path), content, "{args}");
        for change in &changes {
            assert_eq!(change.revision, Some(revision), "{args} {changes:?}");
        }
        let log = s.history.history(&path).await.unwrap();
        assert_eq!(
            log.revisions.len(),
            3,
            "{args} recorded {:?}",
            log.revisions
        );
        assert_eq!(log.current, revision);
    }
    // Every revision is still reachable.
    s.history.restore(&path, 3).await.unwrap();
    assert_eq!(read(&path), "3");
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_that_cannot_run_programs_report_from_history_without_scanning() {
    let s = setup();
    let path = s.project.join("n.txt");
    let changes = call(
        &s,
        "notes.write",
        json!({ "path": "n.txt", "content": "one\n" }),
    )
    .await;
    let change = one(&changes, "n.txt");
    assert_eq!(change.operation, ChangeKind::Created);
    assert_eq!(change.revision, Some(1));
    assert!(change.diff.unwrap().contains("+one"));
    std::fs::write(&path, "edited\n").unwrap();
    let changes = call(
        &s,
        "notes.write",
        json!({ "path": "n.txt", "content": "two\n" }),
    )
    .await;
    let diff = one(&changes, "n.txt").diff.unwrap();
    assert!(diff.contains("-edited") && diff.contains("+two"), "{diff}");
    assert_eq!(s.baseline.scans(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_separate_call_into_a_held_folder_does_not_wait_long() {
    let s = setup();
    call(
        &s,
        "files.write",
        json!({ "path": "seed.txt", "content": "s" }),
    )
    .await;
    let slow = call(
        &s,
        "files.slow_write",
        json!({ "path": "slow.txt", "content": "s", "wait": 3500 }),
    );
    let quick = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let started = Instant::now();
        let changes = call(
            &s,
            "files.write",
            json!({ "path": "quick.txt", "content": "q" }),
        )
        .await;
        (changes, started.elapsed())
    };
    let (slow, (quick, waited)) = tokio::join!(slow, quick);
    assert!(waited < Duration::from_millis(3000), "waited {waited:?}");
    assert!(quick.is_empty(), "{quick:?}");
    one(&slow, "slow.txt");
    let notes: Vec<_> = s
        .trace
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| e.note.clone())
        .collect();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("another call was changing this folder")),
        "{notes:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_crossing_into_each_others_folders_both_finish() {
    let s = setup_with(MockProvider::new().with_script(vec![
        MockProvider::tool_call(
            "c1",
            "files.write",
            json!({ "path": "in.txt", "content": "1" }),
        ),
        MockProvider::text_only("done"),
        MockProvider::tool_call(
            "c2",
            "files.write",
            json!({ "path": "in.txt", "content": "2" }),
        ),
        MockProvider::text_only("done"),
    ]));
    let into_other = call_as(
        &s,
        caller_in(&s.project),
        "files.delegate",
        json!({ "runner": "helper_y", "wait": 300 }),
    );
    let into_project = call_as(
        &s,
        caller_in(&s.other),
        "files.delegate",
        json!({ "runner": "helper_x", "wait": 300 }),
    );
    let (a, b) = tokio::join!(into_other, into_project);
    one(&a, "outer.txt");
    one(&b, "outer.txt");
}

#[tokio::test(flavor = "multi_thread")]
async fn diffs_hide_secrets_the_run_read() {
    let s = setup_with(MockProvider::new().with_script(vec![
        MockProvider::tool_call("c1", "files.secret_write", json!({ "path": "env.txt" })),
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
    assert!(read(&s.project.join("env.txt")).contains(SECRET));
    let mut diff = None;
    while let Ok(event) = rx.try_recv() {
        if let agentd_ai::StreamEvent::ToolResult { changes, .. } = event {
            diff = changes.first().and_then(|c| c.diff.clone());
        }
    }
    let diff = diff.expect("a diff");
    assert!(!diff.contains(SECRET), "{diff}");
    assert!(diff.contains("token"), "{diff}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_with_no_session_folder_captures_nothing() {
    let s = setup();
    let changes = call_as(
        &s,
        Caller::interface("test"),
        "files.write",
        json!({ "path": s.project.join("nowhere.txt"), "content": "x" }),
    )
    .await;
    assert!(changes.is_empty(), "{changes:?}");
    assert_eq!(s.baseline.scans(), 0);
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

/// A chat caller, so `ctx.fs.read` and `ctx.fs.edit` share a read scope.
fn session_caller(s: &Setup) -> Caller {
    let mut caller = caller(s);
    caller.session_id = Some("chat".into());
    caller
}

/// Write a script, let a program touch it in a later captured call, then
/// edit, write and remove it through `ctx.fs`.
async fn change_after_program(script: &str) {
    let s = setup();
    let path = s.project.join("demo.sh");
    call(
        &s,
        "files.write",
        json!({ "path": "demo.sh", "content": "echo one\n" }),
    )
    .await;
    call(&s, "files.sh", json!({ "script": script })).await;
    call_as(
        &s,
        session_caller(&s),
        "files.read_edit",
        json!({ "path": "demo.sh", "old": "one", "new": "two" }),
    )
    .await;
    assert_eq!(read(&path), "echo two\n");
    call(
        &s,
        "files.write",
        json!({ "path": "demo.sh", "content": "echo three\n" }),
    )
    .await;
    s.history.undo(&path).await.unwrap();
    assert_eq!(read(&path), "echo two\n");
    call(&s, "files.remove", json!({ "path": "demo.sh" })).await;
    assert!(!path.exists());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_script_made_executable_by_a_program_can_still_be_changed() {
    change_after_program("chmod +x demo.sh && ./demo.sh").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_run_by_a_program_can_still_be_changed() {
    change_after_program("sh demo.sh").await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_permission_change_by_the_user_keeps_the_file_changeable() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup();
    let path = s.project.join("a.txt");
    call(
        &s,
        "files.write",
        json!({ "path": "a.txt", "content": "first" }),
    )
    .await;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    call(
        &s,
        "files.write",
        json!({ "path": "a.txt", "content": "second" }),
    )
    .await;
    s.history.undo(&path).await.unwrap();
    assert_eq!(read(&path), "first");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_saved_session_keeps_the_files_each_call_changed() {
    use agentd_sessions::{NewSession, Scope, SessionStore};
    let s = setup_with(MockProvider::new().with_script(vec![
        MockProvider::tool_call(
            "c1",
            "files.write",
            json!({ "path": "note.txt", "content": "hello\n" }),
        ),
        MockProvider::text_only("done"),
    ]));
    let session = s
        .sessions
        .create(NewSession {
            cwd: Some(s.project.to_string_lossy().into_owned()),
            ..NewSession::in_scope(&Scope::from_caller(Some("test"), None, None))
        })
        .unwrap();
    s.exec
        .run_runner_with_options(
            Caller::interface("test"),
            "helper",
            agentd_runners::RunOptions {
                prompt: Some("write it".into()),
                session_id: Some(session.id.clone()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let turns = s.sessions.turns(&session.id).unwrap();
    let tool = turns
        .iter()
        .find(|m| m.tool_call_id.as_deref() == Some("c1"))
        .expect("the saved tool message");
    let change = one(&tool.changes, "note.txt");
    assert_eq!(change.operation, ChangeKind::Created);
    assert!(change.diff.as_deref().unwrap().contains("+hello"));
}

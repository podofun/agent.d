//! Session-backed runs: the executor loads a session's turns as history,
//! appends what the run produced, and compacts long sessions on its own.

use std::sync::{Arc, Mutex};

use agentd_ai::{LoopMode, MockProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{Caller, Engine, Grants, GrantsFile, RunnerGrants, model::PermissionSet};
use agentd_runners::{CompactPolicy, RunOptions, RunnerDef, RunnerError, RunnerRegistry};
use agentd_services::ServiceRegistry;
use agentd_sessions::{MemSessionStore, NewSession, Scope, SessionStore};
use agentd_skills::SkillRegistry;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{ActionCall, ActionResult, CallContext, Registry, RegistryError};
use async_trait::async_trait;

struct NoActions;
#[async_trait]
impl Registry for NoActions {
    fn list(&self) -> Vec<String> {
        vec![]
    }
    async fn call(
        &self,
        _ctx: CallContext,
        call: ActionCall,
    ) -> Result<ActionResult, RegistryError> {
        Err(RegistryError::NotFound(call.action))
    }
}

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

/// Records every request it sees and replies from a script.
struct Recording {
    seen: Mutex<Vec<agentd_ai::CompletionRequest>>,
    replies: Mutex<Vec<String>>,
    mode: LoopMode,
}

#[async_trait]
impl agentd_ai::Provider for Recording {
    fn name(&self) -> &str {
        "mock"
    }
    fn loop_mode(&self) -> LoopMode {
        self.mode
    }
    async fn complete(
        &self,
        req: agentd_ai::CompletionRequest,
    ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
        self.seen.lock().unwrap().push(req);
        let mut r = self.replies.lock().unwrap();
        let text = if r.is_empty() {
            "ok".to_string()
        } else {
            r.remove(0)
        };
        Ok(MockProvider::text_only(text))
    }
}

fn build(
    provider: Arc<dyn agentd_ai::Provider>,
    compact: Option<CompactPolicy>,
) -> (Arc<Executor>, Arc<MemSessionStore>) {
    build_with_turns(provider, compact, Executor::DEFAULT_MAX_RUNNER_TURNS)
}

fn build_with_turns(
    provider: Arc<dyn agentd_ai::Provider>,
    compact: Option<CompactPolicy>,
    max_turns: u32,
) -> (Arc<Executor>, Arc<MemSessionStore>) {
    let mut file = GrantsFile::default();
    file.runner.insert(
        "chat".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
            ..Default::default()
        },
    );
    let engine = Arc::new(Engine::new(Grants::from_file(file)));
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "chat".into(),
        model: Some("mock/test".into()),
        compact,
        ..Default::default()
    });
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", provider);
    providers.set_default("mock");
    let mut exec = Executor::new(
        Arc::new(NoActions),
        Arc::new(NullSink),
        engine,
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    );
    let store = Arc::new(MemSessionStore::new());
    exec.set_sessions(store.clone());
    exec.set_max_runner_turns(max_turns);
    (Arc::new(exec), store)
}

fn recording(mode: LoopMode, replies: &[&str]) -> Arc<Recording> {
    Arc::new(Recording {
        seen: Mutex::new(vec![]),
        replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
        mode,
    })
}

fn caller() -> Caller {
    Caller::interface("ws").with_runner("chat")
}

/// A session owned by the same interface `caller()` connects as.
fn ws_session() -> NewSession {
    NewSession::in_scope(&Scope::from_caller(Some("ws"), None, None))
}

fn opts(session_id: &str, prompt: &str) -> RunOptions {
    RunOptions {
        prompt: Some(prompt.into()),
        session_id: Some(session_id.into()),
        ..Default::default()
    }
}

fn contents(msgs: &[agentd_ai::Message]) -> Vec<String> {
    msgs.iter().map(|m| m.content.clone()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn session_history_is_loaded_and_appended() {
    let p = recording(LoopMode::ExecutorOwned, &["hello alice", "you said hi"]);
    let (exec, store) = build(p.clone(), None);
    let s = store.create(ws_session()).unwrap();

    let out = exec
        .run_runner_with_options(caller(), "chat", opts(&s.id, "hi, I'm alice"), None)
        .await
        .unwrap();
    assert_eq!(out.text, "hello alice");
    assert_eq!(out.session_id.as_deref(), Some(s.id.as_str()));

    exec.run_runner_with_options(caller(), "chat", opts(&s.id, "what did I say?"), None)
        .await
        .unwrap();

    let seen = p.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    // Second request carried the first exchange as history.
    assert_eq!(
        contents(&seen[1].messages),
        ["hi, I'm alice", "hello alice", "what did I say?"]
    );
    let stored = store.turns(&s.id).unwrap();
    assert_eq!(
        contents(&stored),
        [
            "hi, I'm alice",
            "hello alice",
            "what did I say?",
            "you said hi"
        ]
    );
    assert_eq!(store.get(&s.id).unwrap().unwrap().turn_count, 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_limit_keeps_the_work_so_far_in_the_session() {
    let looping = MockProvider::new().with_script(vec![
        MockProvider::tool_call("c1", "notes.lookup", serde_json::json!({})),
        MockProvider::tool_call("c2", "notes.lookup", serde_json::json!({})),
        MockProvider::text_only("picked up where I left off"),
    ]);
    let (exec, store) = build_with_turns(Arc::new(looping), None, 2);
    let s = store.create(ws_session()).unwrap();

    let err = exec
        .run_runner_with_options(caller(), "chat", opts(&s.id, "dig in"), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, RunnerError::TurnLimit { limit: 2, .. }),
        "{err}"
    );
    assert_eq!(
        err.to_string(),
        "the runner used all 2 turns without finishing"
    );

    let stored = store.turns(&s.id).unwrap();
    assert_eq!(stored.first().unwrap().content, "dig in");
    assert_eq!(
        stored.iter().filter(|m| m.tool_call_id.is_some()).count(),
        2,
        "both tool results are kept"
    );
    assert_eq!(stored.last().unwrap().role, agentd_ai::Role::Assistant);

    let out = exec
        .run_runner_with_options(caller(), "chat", opts(&s.id, "continue"), None)
        .await
        .unwrap();
    assert_eq!(out.text, "picked up where I left off");
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_owned_persists_prompt_and_reply() {
    let p = recording(LoopMode::ProviderOwned, &["reply one"]);
    let (exec, store) = build(p, None);
    let s = store.create(ws_session()).unwrap();
    exec.run_runner_with_options(caller(), "chat", opts(&s.id, "first"), None)
        .await
        .unwrap();
    assert_eq!(
        contents(&store.turns(&s.id).unwrap()),
        ["first", "reply one"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_session_is_an_error_and_writes_nothing() {
    let p = recording(LoopMode::ExecutorOwned, &[]);
    let (exec, _store) = build(p.clone(), None);
    let err = exec
        .run_runner_with_options(caller(), "chat", opts("nope", "hi"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, RunnerError::SessionNotFound(id) if id == "nope"));
    assert!(p.seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn session_and_messages_are_mutually_exclusive() {
    let p = recording(LoopMode::ExecutorOwned, &[]);
    let (exec, store) = build(p, None);
    let s = store.create(ws_session()).unwrap();
    let mut o = opts(&s.id, "hi");
    o.messages = Some(vec![agentd_ai::Message::user("x")]);
    let err = exec
        .run_runner_with_options(caller(), "chat", o, None)
        .await
        .unwrap_err();
    assert!(matches!(err, RunnerError::InvalidInput(_)), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_run_leaves_session_untouched() {
    struct Failing;
    #[async_trait]
    impl agentd_ai::Provider for Failing {
        fn name(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _req: agentd_ai::CompletionRequest,
        ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
            Err(agentd_ai::ProviderError::Upstream("boom".into()))
        }
    }
    let mut file = GrantsFile::default();
    file.runner.insert(
        "chat".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
            ..Default::default()
        },
    );
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "chat".into(),
        model: Some("mock/test".into()),
        ..Default::default()
    });
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(Failing));
    let mut exec = Executor::new(
        Arc::new(NoActions),
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    );
    let store = Arc::new(MemSessionStore::new());
    exec.set_sessions(store.clone());
    let exec = Arc::new(exec);
    let s = store.create(ws_session()).unwrap();
    exec.run_runner_with_options(caller(), "chat", opts(&s.id, "hi"), None)
        .await
        .unwrap_err();
    assert!(store.turns(&s.id).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_runs_automatically_with_default_policy() {
    // Default policy: after 60 turns keep 20. Seed 60 stored turns, then run.
    let p = recording(LoopMode::ExecutorOwned, &["SUMMARY TEXT", "answer"]);
    let (exec, store) = build(p.clone(), None);
    let s = store.create(ws_session()).unwrap();
    let seed: Vec<agentd_ai::Message> = (0..60)
        .map(|i| {
            if i % 2 == 0 {
                agentd_ai::Message::user(format!("u{i}"))
            } else {
                agentd_ai::Message::assistant(format!("a{i}"))
            }
        })
        .collect();
    store.append(&s.id, &seed).unwrap();

    exec.run_runner_with_options(caller(), "chat", opts(&s.id, "now"), None)
        .await
        .unwrap();

    let seen = p.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "one summary call + one real run");
    // Summary request: the 40 oldest turns + the compaction prompt, no tools.
    assert_eq!(seen[0].messages.len(), 41);
    assert!(seen[0].tools.is_empty());
    assert_eq!(seen[0].messages[40].content, CompactPolicy::DEFAULT_PROMPT);
    // Real run: summary + 20 kept turns + new prompt.
    let run = contents(&seen[1].messages);
    assert_eq!(run.len(), 22);
    assert!(run[0].starts_with("[Conversation summary]\nSUMMARY TEXT"));
    assert_eq!(run[1], "u40");
    assert_eq!(run[21], "now");

    let meta = store.get(&s.id).unwrap().unwrap();
    assert_eq!(meta.compactions, 1);
    assert_eq!(meta.turn_count, 23);
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_can_be_disabled_per_runner() {
    let p = recording(LoopMode::ExecutorOwned, &["answer"]);
    let (exec, store) = build(
        p.clone(),
        Some(CompactPolicy {
            enabled: false,
            ..Default::default()
        }),
    );
    let s = store.create(ws_session()).unwrap();
    let seed: Vec<agentd_ai::Message> = (0..80)
        .map(|i| agentd_ai::Message::user(format!("u{i}")))
        .collect();
    store.append(&s.id, &seed).unwrap();
    exec.run_runner_with_options(caller(), "chat", opts(&s.id, "now"), None)
        .await
        .unwrap();
    assert_eq!(p.seen.lock().unwrap().len(), 1);
    assert_eq!(store.get(&s.id).unwrap().unwrap().compactions, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_never_splits_a_tool_call_group() {
    // Policy: after 6 turns keep 2. History ends in a tool-call group, so the
    // cut must move back to the user turn that started it.
    let p = recording(LoopMode::ExecutorOwned, &["S", "answer"]);
    let (exec, store) = build(
        p.clone(),
        Some(CompactPolicy {
            after_turns: 6,
            keep_recent: 2,
            ..Default::default()
        }),
    );
    let s = store.create(ws_session()).unwrap();
    let call = agentd_ai::ToolCall {
        id: "c1".into(),
        name: "notes.lookup".into(),
        arguments: serde_json::json!({}),
    };
    store
        .append(
            &s.id,
            &[
                agentd_ai::Message::user("u0"),
                agentd_ai::Message::assistant("a1"),
                agentd_ai::Message::user("u2"),
                agentd_ai::Message {
                    role: agentd_ai::Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![call],
                    tool_call_id: None,
                },
                agentd_ai::Message::tool_result("c1", "{}"),
                agentd_ai::Message::assistant("a5"),
            ],
        )
        .unwrap();
    exec.run_runner_with_options(caller(), "chat", opts(&s.id, "now"), None)
        .await
        .unwrap();
    let seen = p.seen.lock().unwrap();
    // Naive cut would be 4 (keep a5 + tool result). Safe cut is 2 (keep from u2).
    assert_eq!(contents(&seen[0].messages[..2]), ["u0", "a1"]);
    let run = contents(&seen[1].messages);
    assert_eq!(run[1], "u2");
    assert_eq!(run.len(), 6);
}

#[tokio::test(flavor = "multi_thread")]
async fn summary_failure_falls_back_to_full_history() {
    // Script has only one reply; the summary call consumes... no: make the
    // summary come back empty so compaction is skipped.
    let p = recording(LoopMode::ExecutorOwned, &["", "answer"]);
    let (exec, store) = build(
        p.clone(),
        Some(CompactPolicy {
            after_turns: 4,
            keep_recent: 2,
            ..Default::default()
        }),
    );
    let s = store.create(ws_session()).unwrap();
    let seed: Vec<agentd_ai::Message> = (0..4)
        .map(|i| agentd_ai::Message::user(format!("u{i}")))
        .collect();
    store.append(&s.id, &seed).unwrap();
    let out = exec
        .run_runner_with_options(caller(), "chat", opts(&s.id, "now"), None)
        .await
        .unwrap();
    assert_eq!(out.text, "answer");
    let seen = p.seen.lock().unwrap();
    assert_eq!(seen[1].messages.len(), 5, "full history + prompt");
    assert_eq!(store.get(&s.id).unwrap().unwrap().compactions, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_runs_on_one_session_are_refused() {
    struct Slow;
    #[async_trait]
    impl agentd_ai::Provider for Slow {
        fn name(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _req: agentd_ai::CompletionRequest,
        ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            Ok(MockProvider::text_only("slow"))
        }
    }
    let mut file = GrantsFile::default();
    file.runner.insert(
        "chat".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
            ..Default::default()
        },
    );
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "chat".into(),
        model: Some("mock/test".into()),
        ..Default::default()
    });
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(Slow));
    let mut exec = Executor::new(
        Arc::new(NoActions),
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    );
    let store = Arc::new(MemSessionStore::new());
    exec.set_sessions(store.clone());
    let exec = Arc::new(exec);
    let s = store.create(ws_session()).unwrap();
    let a = {
        let exec = exec.clone();
        let id = s.id.clone();
        tokio::spawn(async move {
            exec.run_runner_with_options(caller(), "chat", opts(&id, "one"), None)
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let b = exec
        .run_runner_with_options(caller(), "chat", opts(&s.id, "two"), None)
        .await;
    assert!(matches!(b, Err(RunnerError::SessionBusy(_))), "{b:?}");
    a.await.unwrap().unwrap();
    // Guard released: a third run goes through.
    exec.run_runner_with_options(caller(), "chat", opts(&s.id, "three"), None)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn session_outside_callers_scope_reads_as_missing() {
    let p = recording(LoopMode::ExecutorOwned, &["never"]);
    let (exec, store) = build(p.clone(), None);
    // Owned by another interface.
    let other = store
        .create(NewSession::in_scope(&Scope::from_caller(
            Some("telegram"),
            None,
            None,
        )))
        .unwrap();
    let err = exec
        .run_runner_with_options(caller(), "chat", opts(&other.id, "hi"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, RunnerError::SessionNotFound(_)), "{err}");
    // Same interface, but bound to a user the caller does not declare.
    let alices = store
        .create(NewSession::in_scope(&Scope::from_caller(
            Some("ws"),
            None,
            Some("alice"),
        )))
        .unwrap();
    let err = exec
        .run_runner_with_options(caller(), "chat", opts(&alices.id, "hi"), None)
        .await
        .unwrap_err();
    assert!(matches!(err, RunnerError::SessionNotFound(_)), "{err}");
    // Declaring the user unlocks it.
    exec.run_runner_with_options(
        caller().with_user("alice"),
        "chat",
        opts(&alices.id, "hi"),
        None,
    )
    .await
    .unwrap();
    assert!(p.seen.lock().unwrap().len() == 1);
    assert!(store.turns(&other.id).unwrap().is_empty());
}

//! End-to-end tool-use loop test. A scripted `MockProvider` emits one
//! `tool_call`, then a plain-text reply. The executor must dispatch the
//! tool through its full permission engine, append the result to the
//! conversation, and re-call the provider until it returns text.

use std::sync::{Arc, Mutex};

use agentd_ai::{LoopMode, MockProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{
    Caller, Engine, Grants, GrantsFile, RunnerGrants, ToolGrants, model::PermissionSet,
};
use agentd_runners::{RunnerDef, RunnerRegistry};
use agentd_services::ServiceRegistry;
use agentd_skills::SkillRegistry;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, CallContext, Registry, RegistryActionInfo, RegistryError,
    RegistryToolInfo,
};
use async_trait::async_trait;

/// In-process action `notes.lookup` that returns a fixed snippet. We log
/// every `call` invocation so the test can assert the executor actually
/// dispatched (versus the model hallucinating that it did).
struct FakeNotes {
    calls: Arc<Mutex<Vec<ActionCall>>>,
}

#[async_trait]
impl Registry for FakeNotes {
    fn list(&self) -> Vec<String> {
        vec!["notes.lookup".into()]
    }
    fn action_info(&self, name: &str) -> Option<RegistryActionInfo> {
        if name == "notes.lookup" {
            Some(RegistryActionInfo {
                name: name.into(),
                tool: Some("notes".into()),
                requires: vec!["notes.read".into()],
                confirm: false,
                input_schema: None,
            })
        } else {
            None
        }
    }
    fn tool_info(&self, name: &str) -> Option<RegistryToolInfo> {
        if name == "notes" {
            Some(RegistryToolInfo {
                name: "notes".into(),
                requires: vec!["notes.read".into()],
            })
        } else {
            None
        }
    }
    async fn call(
        &self,
        _ctx: CallContext,
        call: ActionCall,
    ) -> Result<ActionResult, RegistryError> {
        self.calls.lock().unwrap().push(call.clone());
        Ok(ActionResult {
            value: serde_json::json!({ "found": "the answer is 42" }),
        })
    }
}

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

fn build_executor(
    provider: Arc<dyn agentd_ai::Provider>,
) -> (Arc<Executor>, Arc<Mutex<Vec<ActionCall>>>) {
    let calls: Arc<Mutex<Vec<ActionCall>>> = Arc::default();
    let registry: Arc<dyn Registry> = Arc::new(FakeNotes {
        calls: calls.clone(),
    });

    // Grants: tool "notes" gets `notes.read`; runner "researcher" may call
    // `notes.lookup`. Anything else would be denied by the engine.
    let mut file = GrantsFile::default();
    file.tool.insert(
        "notes".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["notes.read"]),
        },
    );
    let mut runner_grants = RunnerGrants {
        granted: PermissionSet::from_iter(["ai:mock"]),
        ..Default::default()
    };
    runner_grants.allowed_actions.insert("notes.lookup".into());
    file.runner.insert("researcher".into(), runner_grants);
    let engine = Arc::new(Engine::new(Grants::from_file(file)));

    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "researcher".into(),
        system: Some("You answer using notes.lookup when needed.".into()),
        model: Some("mock/test".into()),
        allowed_actions: vec!["notes.lookup".into()],
        ..Default::default()
    });

    let mut providers = ProviderRegistry::new();
    providers.insert("mock", provider);
    providers.set_default("mock");

    let exec = Arc::new(Executor::new(
        registry,
        Arc::new(NullSink),
        engine,
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    ));
    (exec, calls)
}

#[tokio::test(flavor = "multi_thread")]
async fn executor_runs_tool_call_and_feeds_result_back() {
    // Mock script: turn 1 = tool_call(notes.lookup, {"q":"42"}); turn 2 =
    // plain text reply using the result.
    let mock = MockProvider::new().with_script(vec![
        MockProvider::tool_call("call_1", "notes.lookup", serde_json::json!({ "q": "42" })),
        MockProvider::text_only("notes say: the answer is 42"),
    ]);
    let (exec, calls) = build_executor(Arc::new(mock));

    let outcome = exec
        .run_runner(
            Caller::interface("ws").with_runner("researcher"),
            "researcher",
            "what is the answer?".into(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.text, "notes say: the answer is 42");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "executor should have dispatched once");
    assert_eq!(calls[0].action, "notes.lookup");
    assert_eq!(calls[0].args, serde_json::json!({ "q": "42" }));
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_call_denied_when_runner_lacks_allowlist() {
    // Same model script, but runner "researcher" never gets the action
    // allowlist entry. The executor dispatches; engine denies; the tool
    // result fed back to the model carries the deny message; second turn
    // produces a "couldn't read" reply. This proves errors are recoverable
    // rather than fatal to the runner.
    let mock = MockProvider::new().with_script(vec![
        MockProvider::tool_call("call_1", "notes.lookup", serde_json::json!({})),
        MockProvider::text_only("could not read notes"),
    ]);

    // Build an executor where the runner has NO allowed_actions in grants.
    let calls: Arc<Mutex<Vec<ActionCall>>> = Arc::default();
    let registry: Arc<dyn Registry> = Arc::new(FakeNotes {
        calls: calls.clone(),
    });
    let mut file = GrantsFile::default();
    file.tool.insert(
        "notes".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["notes.read"]),
        },
    );
    file.runner.insert(
        "researcher".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
            ..Default::default()
        },
    );
    let engine = Arc::new(Engine::new(Grants::from_file(file)));
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "researcher".into(),
        model: Some("mock/test".into()),
        allowed_actions: vec!["notes.lookup".into()],
        ..Default::default()
    });
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(mock) as Arc<dyn agentd_ai::Provider>);
    providers.set_default("mock");
    let exec = Arc::new(Executor::new(
        registry,
        Arc::new(NullSink),
        engine,
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    ));

    let outcome = exec
        .run_runner(
            Caller::interface("ws").with_runner("researcher"),
            "researcher",
            "lookup".into(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.text, "could not read notes");
    // FakeNotes::call never ran because the engine layer-3 (runner allow)
    // rejected the dispatch before reaching the registry.
    assert!(
        calls.lock().unwrap().is_empty(),
        "denied tool calls should not reach the registry"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn provider_owned_loop_skips_executor_dispatch() {
    // A `ProviderOwned` provider that handles its own loop returns a final
    // text directly; even if the conversation grew, the executor doesn't
    // re-call complete() or dispatch anything itself.
    struct OwningProvider {
        calls: Mutex<u32>,
    }
    #[async_trait::async_trait]
    impl agentd_ai::Provider for OwningProvider {
        fn name(&self) -> &str {
            "owning"
        }
        fn loop_mode(&self) -> LoopMode {
            LoopMode::ProviderOwned
        }
        async fn complete(
            &self,
            _req: agentd_ai::CompletionRequest,
        ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
            *self.calls.lock().unwrap() += 1;
            Ok(MockProvider::text_only("final answer from provider"))
        }
    }
    let provider = Arc::new(OwningProvider {
        calls: Mutex::new(0),
    });
    let (exec, registry_calls) = build_executor(provider.clone() as Arc<dyn agentd_ai::Provider>);
    let outcome = exec
        .run_runner(
            Caller::interface("ws").with_runner("researcher"),
            "researcher",
            "hi".into(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.text, "final answer from provider");
    assert_eq!(*provider.calls.lock().unwrap(), 1);
    assert!(registry_calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_run_emits_deltas_and_returns_complete_outcome() {
    // Two-turn script: a tool call, then the final text. The streaming run
    // must surface deltas from BOTH turns and still return the complete
    // outcome — streaming augments the aggregate contract, never replaces it.
    let mock = MockProvider::new().with_script(vec![
        MockProvider::tool_call("call_1", "notes.lookup", serde_json::json!({ "q": "42" })),
        MockProvider::text_only("notes say: the answer is 42"),
    ]);
    let (exec, calls) = build_executor(Arc::new(mock));

    let (tx, mut rx) = agentd_ai::types::stream_channel();
    let outcome = exec
        .run_runner_streaming(
            Caller::interface("ws").with_runner("researcher"),
            "researcher",
            "what is the answer?".into(),
            tx,
        )
        .await
        .unwrap();

    assert_eq!(outcome.text, "notes say: the answer is 42");
    assert_eq!(calls.lock().unwrap().len(), 1);

    let mut deltas = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        deltas.push(ev);
    }
    // Turn 1: a ToolCall event (empty text → no text deltas). Turn 2: the
    // final text as deltas. TurnEnd after each provider turn.
    assert!(
        deltas.iter().any(
            |e| matches!(e, agentd_ai::StreamEvent::ToolCall { name } if name == "notes.lookup")
        ),
        "missing tool-call event: {deltas:?}"
    );
    let streamed_text: String = deltas
        .iter()
        .filter_map(|e| match e {
            agentd_ai::StreamEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed_text, "notes say: the answer is 42");
    assert_eq!(
        deltas
            .iter()
            .filter(|e| matches!(e, agentd_ai::StreamEvent::TurnEnd))
            .count(),
        2
    );
}

#[tokio::test]
async fn nested_runner_dispatch_enforces_provider_grants() {
    use agentd_types::RunnerDispatcher;
    let (exec, _) = build_executor(Arc::new(MockProvider::new()));
    exec.runners().insert(RunnerDef {
        name: "ungranted".into(),
        model: Some("mock/test".into()),
        ..Default::default()
    });
    let dispatcher = agentd_executor::ExecutorHandle::new(exec);
    let result = dispatcher
        .run_runner_json(
            Caller::service("worker"),
            "ungranted",
            serde_json::json!({"prompt":"hello"}),
        )
        .await;
    assert!(result.unwrap_err().contains("ai:mock"));
    let result = dispatcher
        .run_runner_json(
            Caller::service("worker"),
            "researcher",
            serde_json::json!({"messages":[{"role":"user","content":"hello"}],"max_tokens":64}),
        )
        .await;
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn usage_is_summed_across_turns_and_missing_usage_stays_unknown() {
    for known in [true, false] {
        let mut tool = MockProvider::tool_call("t1", "notes.lookup", serde_json::json!({}));
        tool.usage = Some(agentd_ai::types::Usage {
            input_tokens: 10,
            output_tokens: 3,
            ..Default::default()
        });
        let mut final_reply = MockProvider::text_only("done");
        if known {
            final_reply.usage = Some(agentd_ai::types::Usage {
                input_tokens: 20,
                output_tokens: 5,
                ..Default::default()
            });
        }
        let (exec, _) = build_executor(Arc::new(
            MockProvider::new().with_script(vec![tool, final_reply]),
        ));
        let out = exec
            .run_runner(Caller::interface("ws"), "researcher", "hello".into())
            .await
            .unwrap();
        if known {
            let usage = out.usage.unwrap();
            assert_eq!((usage.input_tokens, usage.output_tokens), (30, 8));
        } else {
            assert!(out.usage.is_none());
        }
    }
}

struct BurstProvider;
#[async_trait]
impl agentd_ai::Provider for BurstProvider {
    fn name(&self) -> &str {
        "mock"
    }
    async fn complete(
        &self,
        _: agentd_ai::CompletionRequest,
    ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
        Ok(MockProvider::tool_call(
            "t1",
            "notes.lookup",
            serde_json::json!({}),
        ))
    }
    async fn complete_streaming(
        &self,
        req: agentd_ai::CompletionRequest,
        sink: agentd_ai::StreamSink,
    ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
        for _ in 0..300 {
            let _ = sink.send(agentd_ai::StreamEvent::TextDelta { text: "x".into() });
        }
        self.complete(req).await
    }
}

#[tokio::test]
async fn overflow_stops_before_dispatching_tools() {
    let (exec, calls) = build_executor(Arc::new(BurstProvider));
    let (sink, _receiver) = agentd_ai::types::stream_channel();
    let result = exec
        .run_runner_streaming(Caller::interface("ws"), "researcher", "hello".into(), sink)
        .await;
    assert!(matches!(
        result,
        Err(agentd_runners::RunnerError::SlowConsumer)
    ));
    assert!(calls.lock().unwrap().is_empty());
}

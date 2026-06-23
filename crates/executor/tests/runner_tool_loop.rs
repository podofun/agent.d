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
    let mut runner_grants = RunnerGrants::default();
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
    file.runner
        .insert("researcher".into(), RunnerGrants::default());
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

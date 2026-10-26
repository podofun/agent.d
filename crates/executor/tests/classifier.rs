//! A run whose approval is `classifier` has its questions answered by the
//! runner `[approval].classifier` names: allow runs the call once, refuse
//! refuses it with the classifier's reason, and unsure goes to a human.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentd_ai::{MockProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{
    ApprovalSetting, Caller, Engine, Grants, GrantsFile, RunnerGrants, model::PermissionSet,
};
use agentd_runners::{RunOptions, RunnerDef, RunnerRegistry};
use agentd_services::ServiceRegistry;
use agentd_skills::SkillRegistry;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, ApprovalBroker, ApprovalKind, ApprovalRequest, CallContext, Registry,
    RegistryActionInfo, RegistryError, Verdict,
};
use async_trait::async_trait;

/// `tool.act` needs `cap:foo`, which its tool is not granted.
#[derive(Default)]
struct OneAction {
    ran: AtomicUsize,
}
#[async_trait]
impl Registry for OneAction {
    fn list(&self) -> Vec<String> {
        vec!["tool.act".into()]
    }
    fn action_info(&self, name: &str) -> Option<RegistryActionInfo> {
        (name == "tool.act").then(|| RegistryActionInfo {
            name: "tool.act".into(),
            tool: Some("tool".into()),
            requires: vec!["cap:foo".into()],
            confirm: false,
            input_schema: None,
        })
    }
    async fn call(
        &self,
        _ctx: CallContext,
        _call: ActionCall,
    ) -> Result<ActionResult, RegistryError> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(ActionResult {
            value: serde_json::json!({ "ok": true }),
        })
    }
}

/// Records every trace event, to read back tool results.
#[derive(Default)]
struct Sink(Mutex<Vec<TraceEvent>>);
#[async_trait]
impl TraceSink for Sink {
    async fn record(&self, e: TraceEvent) {
        self.0.lock().unwrap().push(e);
    }
}

#[derive(Default)]
struct Human {
    asked: Mutex<Vec<ApprovalKind>>,
}
#[async_trait]
impl ApprovalBroker for Human {
    async fn request(&self, req: ApprovalRequest) -> Verdict {
        self.asked.lock().unwrap().push(req.kind);
        Verdict::AllowOnce
    }
}

struct Setup {
    exec: Arc<Executor>,
    registry: Arc<OneAction>,
    human: Arc<Human>,
    sink: Arc<Sink>,
}

/// `coder` has approval `classifier`; `judge` replies `judge_reply`.
/// `allowlisted` puts `tool.act` in coder's `allowed_actions` in grants.toml.
fn setup(judge_reply: &str, configured: bool, allowlisted: bool) -> Setup {
    let mut file = GrantsFile::default();
    file.runner.insert(
        "coder".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
            allowed_actions: if allowlisted {
                ["tool.act".to_string()].into_iter().collect()
            } else {
                Default::default()
            },
            approval: Some(ApprovalSetting::Classifier),
        },
    );
    file.runner.insert(
        "judge".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:judge"]),
            ..Default::default()
        },
    );
    if configured {
        file.approval.classifier = Some("judge".into());
    }
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "coder".into(),
        model: Some("mock/m".into()),
        allowed_actions: vec!["tool.act".into()],
        ..Default::default()
    });
    runners.insert(RunnerDef {
        name: "judge".into(),
        model: Some("judge/m".into()),
        ..Default::default()
    });
    let coder = MockProvider::new().with_script(vec![
        MockProvider::tool_call("c1", "tool.act", serde_json::json!({ "x": 1 })),
        MockProvider::text_only("done"),
    ]);
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(coder));
    providers.insert(
        "judge",
        Arc::new(MockProvider::new().with_reply(judge_reply)),
    );
    let registry = Arc::new(OneAction::default());
    let human = Arc::new(Human::default());
    let sink = Arc::new(Sink::default());
    let mut exec = Executor::new(
        registry.clone(),
        sink.clone(),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    );
    exec.set_broker(human.clone());
    Setup {
        exec: Arc::new(exec),
        registry,
        human,
        sink,
    }
}

async fn run(s: &Setup) {
    s.exec
        .run_runner_with_options(
            Caller::interface("ws").with_runner("coder"),
            "coder",
            RunOptions {
                prompt: Some("go".into()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
}

fn ran(s: &Setup) -> usize {
    s.registry.ran.load(Ordering::SeqCst)
}

fn asked(s: &Setup) -> Vec<ApprovalKind> {
    s.human.asked.lock().unwrap().clone()
}

fn approval_results(s: &Setup) -> Vec<serde_json::Value> {
    s.sink
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.kind.as_deref() == Some("approval"))
        .filter_map(|e| e.result.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_allow_runs_the_call_without_asking_a_human() {
    let s = setup(r#"{"answer":"allow","reason":"harmless"}"#, true, true);
    run(&s).await;
    assert_eq!(ran(&s), 1);
    assert!(asked(&s).is_empty());
    let results = approval_results(&s);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["decided_by"], "classifier");
    assert_eq!(results[0]["reason"], "harmless");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refusal_refuses_with_the_classifiers_reason() {
    let s = setup(r#"{"answer":"refuse","reason":"it publishes"}"#, true, true);
    run(&s).await;
    assert_eq!(ran(&s), 0);
    assert!(asked(&s).is_empty());
    let all = s
        .sink
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("The classifier refused `tool.act`. it publishes."),
        "{all}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unsure_or_an_unreadable_answer_goes_to_a_human() {
    let s = setup(r#"{"answer":"unsure","reason":"cannot tell"}"#, true, true);
    run(&s).await;
    assert_eq!((asked(&s), ran(&s)), (vec![ApprovalKind::MissingGrant], 1));

    let s = setup("I think it is fine", true, true);
    run(&s).await;
    assert_eq!((asked(&s), ran(&s)), (vec![ApprovalKind::MissingGrant], 1));
}

/// A runner calling an action outside its `allowed_actions` is always asked
/// of a human; the grant question after it goes to the classifier.
#[tokio::test(flavor = "multi_thread")]
async fn runner_allowlist_questions_always_go_to_a_human() {
    let s = setup(r#"{"answer":"allow","reason":"fine"}"#, true, false);
    run(&s).await;
    assert_eq!(asked(&s), vec![ApprovalKind::RunnerAction]);
    assert_eq!(ran(&s), 1);
}

/// With no classifier configured, a `classifier` run behaves as `human`.
#[tokio::test(flavor = "multi_thread")]
async fn without_a_classifier_the_run_is_human() {
    let s = setup(r#"{"answer":"allow"}"#, false, true);
    run(&s).await;
    assert_eq!((asked(&s), ran(&s)), (vec![ApprovalKind::MissingGrant], 1));
}

#[test]
fn the_classifier_configuration_is_validated() {
    // A valid setup passes.
    setup("{}", true, true)
        .exec
        .validate_approval_config()
        .unwrap();
    // A runner defaulting to `classifier` needs a classifier.
    let e = setup("{}", false, true)
        .exec
        .validate_approval_config()
        .unwrap_err();
    assert!(
        e.contains("`coder`") && e.contains("defaults to `classifier`"),
        "{e}"
    );
}

fn executor_with_classifier(def: RunnerDef, grants: RunnerGrants, own_tools: bool) -> Executor {
    let mut file = GrantsFile::default();
    file.approval.classifier = Some(def.name.clone());
    file.runner.insert(def.name.clone(), grants);
    let runners = RunnerRegistry::new();
    runners.insert(def);
    let mut providers = ProviderRegistry::new();
    let provider: Arc<dyn agentd_ai::Provider> = if own_tools {
        Arc::new(OwnTools)
    } else {
        Arc::new(MockProvider::new())
    };
    providers.insert("judge", provider);
    Executor::new(
        Arc::new(OneAction::default()),
        Arc::new(Sink::default()),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    )
}

struct OwnTools;
#[async_trait]
impl agentd_ai::Provider for OwnTools {
    fn name(&self) -> &str {
        "judge"
    }
    fn has_own_tools(&self) -> bool {
        true
    }
    async fn complete(
        &self,
        _req: agentd_ai::CompletionRequest,
    ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
        Ok(MockProvider::text_only("{}"))
    }
}

#[test]
fn a_classifier_must_have_no_actions_and_a_driven_provider() {
    let judge = || RunnerDef {
        name: "judge".into(),
        model: Some("judge/m".into()),
        ..Default::default()
    };
    let granted = || RunnerGrants {
        granted: PermissionSet::from_iter(["ai:judge"]),
        ..Default::default()
    };

    let with_lua_actions = executor_with_classifier(
        RunnerDef {
            allowed_actions: vec!["tool.act".into()],
            ..judge()
        },
        granted(),
        false,
    );
    let e = with_lua_actions.validate_approval_config().unwrap_err();
    assert!(e.contains("declares actions"), "{e}");

    let with_grant_actions = executor_with_classifier(
        judge(),
        RunnerGrants {
            allowed_actions: ["tool.act".to_string()].into_iter().collect(),
            ..granted()
        },
        false,
    );
    let e = with_grant_actions.validate_approval_config().unwrap_err();
    assert!(e.contains("`allowed_actions` in grants.toml"), "{e}");

    let own_tools = executor_with_classifier(judge(), granted(), true);
    let e = own_tools.validate_approval_config().unwrap_err();
    assert!(e.contains("runs its own tools"), "{e}");

    executor_with_classifier(judge(), granted(), false)
        .validate_approval_config()
        .unwrap();
}

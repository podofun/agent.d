//! A run's approval setting is resolved once, when it starts, from the
//! runner's default, the session's stored value, and the value passed for the
//! run. `nobody` refuses anything not granted without asking anyone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agentd_ai::{MockProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{
    ApprovalSetting, Caller, Engine, Grants, GrantsFile, RunnerGrants, model::PermissionSet,
};
use agentd_runners::{RunOptions, RunnerDef, RunnerError, RunnerRegistry};
use agentd_services::ServiceRegistry;
use agentd_sessions::{MemSessionStore, NewSession, Scope, SessionStore};
use agentd_skills::SkillRegistry;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, ApprovalBroker, ApprovalRequest, CallContext, Registry,
    RegistryActionInfo, RegistryError, Verdict,
};
use async_trait::async_trait;

/// One action, `tool.act`, needing `cap:foo`, which its tool is not granted.
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

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

/// Allows everything it is asked, and records what it saw.
#[derive(Default)]
struct Approver {
    asked: Mutex<Vec<ApprovalRequest>>,
}
#[async_trait]
impl ApprovalBroker for Approver {
    async fn request(&self, req: ApprovalRequest) -> Verdict {
        self.asked.lock().unwrap().push(req);
        Verdict::AllowOnce
    }
}

struct Setup {
    exec: Arc<Executor>,
    registry: Arc<OneAction>,
    approver: Arc<Approver>,
    store: Arc<MemSessionStore>,
}

fn setup(runner_default: Option<ApprovalSetting>) -> Setup {
    let mut file = GrantsFile::default();
    file.runner.insert(
        "coder".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
            allowed_actions: ["tool.act".to_string()].into_iter().collect(),
            approval: runner_default,
        },
    );
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "coder".into(),
        model: Some("mock/m".into()),
        allowed_actions: vec!["tool.act".into()],
        ..Default::default()
    });
    // Enough for two runs: each asks for `tool.act` once, then answers.
    let model = MockProvider::new().with_script(vec![
        MockProvider::tool_call("c1", "tool.act", serde_json::json!({})),
        MockProvider::text_only("done"),
        MockProvider::tool_call("c2", "tool.act", serde_json::json!({})),
        MockProvider::text_only("done"),
    ]);
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(model));
    let registry = Arc::new(OneAction::default());
    let approver = Arc::new(Approver::default());
    let store = Arc::new(MemSessionStore::new());
    let mut exec = Executor::new(
        registry.clone(),
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    );
    exec.set_broker(approver.clone());
    exec.set_sessions(store.clone());
    Setup {
        exec: Arc::new(exec),
        registry,
        approver,
        store,
    }
}

fn caller() -> Caller {
    Caller::interface("ws").with_runner("coder")
}

fn opts() -> RunOptions {
    RunOptions {
        prompt: Some("go".into()),
        ..Default::default()
    }
}

async fn run(s: &Setup, options: RunOptions) -> Result<String, RunnerError> {
    s.exec
        .run_runner_with_options(caller(), "coder", options, None)
        .await
        .map(|o| o.text)
}

fn asked(s: &Setup) -> usize {
    s.approver.asked.lock().unwrap().len()
}

fn ran(s: &Setup) -> usize {
    s.registry.ran.load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread")]
async fn human_asks_and_carries_the_run_ids_on_the_request() {
    let s = setup(None);
    run(&s, opts()).await.unwrap();
    assert_eq!((asked(&s), ran(&s)), (1, 1));
    let req = &s.approver.asked.lock().unwrap()[0];
    assert_eq!(req.caller.approval, Some(ApprovalSetting::Human));
    assert!(req.caller.run_id.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_nobody_runner_refuses_without_asking() {
    let s = setup(Some(ApprovalSetting::Nobody));
    assert_eq!(run(&s, opts()).await.unwrap(), "done");
    assert_eq!((asked(&s), ran(&s)), (0, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_may_tighten_to_nobody() {
    let s = setup(None);
    let o = RunOptions {
        approval: Some(ApprovalSetting::Nobody),
        ..opts()
    };
    run(&s, o).await.unwrap();
    assert_eq!((asked(&s), ran(&s)), (0, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_an_admin_may_loosen_a_run() {
    let s = setup(Some(ApprovalSetting::Nobody));
    let looser = RunOptions {
        approval: Some(ApprovalSetting::Human),
        ..opts()
    };
    let err = run(&s, looser.clone()).await.unwrap_err();
    assert!(
        matches!(err, RunnerError::ApprovalForbidden { .. }),
        "{err}"
    );
    assert_eq!(ran(&s), 0);

    run(
        &s,
        RunOptions {
            admin: true,
            ..looser
        },
    )
    .await
    .unwrap();
    assert_eq!((asked(&s), ran(&s)), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_looser_session_value_applies_only_to_an_admin_run() {
    let s = setup(Some(ApprovalSetting::Nobody));
    let session = s
        .store
        .create(
            NewSession::in_scope(&Scope::from_caller(Some("ws"), None, None))
                .approval(Some(ApprovalSetting::Human)),
        )
        .unwrap();
    let in_session = RunOptions {
        session_id: Some(session.id.clone()),
        ..opts()
    };
    run(&s, in_session.clone()).await.unwrap();
    assert_eq!((asked(&s), ran(&s)), (0, 0), "not applied for a non-admin");

    let admin_session = s
        .store
        .create(NewSession::in_scope(&Scope::admin(None)).approval(Some(ApprovalSetting::Human)))
        .unwrap();
    run(
        &s,
        RunOptions {
            session_id: Some(admin_session.id.clone()),
            admin: true,
            ..opts()
        },
    )
    .await
    .unwrap();
    assert_eq!((asked(&s), ran(&s)), (1, 1), "applied for an admin");
    let req = &s.approver.asked.lock().unwrap()[0];
    assert_eq!(
        req.caller.session_id.as_deref(),
        Some(admin_session.id.as_str())
    );
}

/// The run's setting and ids are set by the executor only; a caller read
/// from the wire never carries them.
#[test]
fn the_caller_fields_cannot_be_set_from_the_wire() {
    let c: Caller = serde_json::from_value(serde_json::json!({
        "interface": "ws",
        "approval": "human",
        "run_id": "run-1",
        "session_id": "s-1",
    }))
    .unwrap();
    assert_eq!((c.approval, c.run_id, c.session_id), (None, None, None));
}

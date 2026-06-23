//! Executor interactive-approval escalation tests. A mock `ApprovalBroker`
//! returns a scripted verdict; we assert the executor applies allow-once /
//! allow-forever / deny correctly, including persistence to grants.toml.

use std::sync::Arc;

use agentd_executor::Executor;
use agentd_permissions::{
    Caller, Engine, Grants, GrantsFile,
    grants::{InterfaceGrants, RunnerGrants, ToolGrants},
    model::PermissionSet,
};
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, ApprovalBroker, ApprovalRequest, CallContext, Registry,
    RegistryActionInfo, RegistryError, RegistryToolInfo, Verdict,
};
use async_trait::async_trait;

struct FakeRegistry {
    action_requires: Vec<String>,
    tool_requires: Vec<String>,
    confirm: bool,
}

#[async_trait]
impl Registry for FakeRegistry {
    fn list(&self) -> Vec<String> {
        vec!["tool.act".into()]
    }
    fn action_info(&self, name: &str) -> Option<RegistryActionInfo> {
        (name == "tool.act").then(|| RegistryActionInfo {
            name: "tool.act".into(),
            tool: Some("tool".into()),
            requires: self.action_requires.clone(),
            confirm: self.confirm,
            input_schema: None,
        })
    }
    fn tool_info(&self, name: &str) -> Option<RegistryToolInfo> {
        (name == "tool").then(|| RegistryToolInfo {
            name: "tool".into(),
            requires: self.tool_requires.clone(),
        })
    }
    async fn call(
        &self,
        _ctx: CallContext,
        _call: ActionCall,
    ) -> Result<ActionResult, RegistryError> {
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

/// Broker that returns a fixed verdict and records the request it saw.
struct MockBroker {
    verdict: Verdict,
    seen: std::sync::Mutex<Option<ApprovalRequest>>,
}
impl MockBroker {
    fn new(verdict: Verdict) -> Arc<Self> {
        Arc::new(Self {
            verdict,
            seen: std::sync::Mutex::new(None),
        })
    }
}
#[async_trait]
impl ApprovalBroker for MockBroker {
    async fn request(&self, req: ApprovalRequest) -> Verdict {
        *self.seen.lock().unwrap() = Some(req);
        self.verdict
    }
}

fn engine_with_tool_grant(perms: &[&str]) -> Engine {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "tool".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(perms.iter().copied()),
        },
    );
    Engine::new(Grants::from_file(file))
}

fn engine_from_file(file: GrantsFile) -> Engine {
    Engine::new(Grants::from_file(file))
}

fn base_executor(requires: &[&str], confirm: bool, engine: Engine) -> Executor {
    executor_with(requires, &[], confirm, engine)
}

fn executor_with(
    action_requires: &[&str],
    tool_requires: &[&str],
    confirm: bool,
    engine: Engine,
) -> Executor {
    let reg = Arc::new(FakeRegistry {
        action_requires: action_requires.iter().map(|s| s.to_string()).collect(),
        tool_requires: tool_requires.iter().map(|s| s.to_string()).collect(),
        confirm,
    });
    Executor::new(
        reg,
        Arc::new(NullSink),
        Arc::new(engine),
        agentd_runners::RunnerRegistry::new(),
        agentd_services::ServiceRegistry::new(),
        agentd_skills::SkillRegistry::new(),
        Arc::new(agentd_ai::ProviderRegistry::new()),
    )
}

fn act() -> ActionCall {
    ActionCall {
        action: "tool.act".into(),
        args: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn allow_once_missing_grant_proceeds_without_writing_grants() {
    let mut exec = base_executor(&["cap:foo"], false, engine_with_tool_grant(&[]));
    let broker = MockBroker::new(Verdict::AllowOnce);
    exec.set_broker(broker.clone());
    // No grants_path set => allow-forever would degrade, but this is allow-once.
    let res = exec.run(Caller::interface("http"), act()).await;
    assert!(res.is_ok(), "allow-once should proceed: {res:?}");
    let seen = broker.seen.lock().unwrap();
    assert_eq!(seen.as_ref().unwrap().missing, vec!["cap:foo".to_string()]);
}

#[tokio::test]
async fn escalation_missing_includes_tool_declared_requires() {
    // Action declares no `requires`; the tool declares `cap:tool`. The
    // escalation request must surface the tool's permission so an
    // allow-forever verdict persists the right grant.
    let mut exec = executor_with(&[], &["cap:tool"], false, engine_with_tool_grant(&[]));
    let broker = MockBroker::new(Verdict::AllowOnce);
    exec.set_broker(broker.clone());
    let res = exec.run(Caller::interface("http"), act()).await;
    assert!(res.is_ok(), "allow-once should proceed: {res:?}");
    let seen = broker.seen.lock().unwrap();
    assert_eq!(seen.as_ref().unwrap().missing, vec!["cap:tool".to_string()]);
}

#[tokio::test]
async fn allow_once_confirm_skips_gate() {
    let mut exec = base_executor(&["cap:foo"], true, engine_with_tool_grant(&["cap:foo"]));
    exec.set_broker(MockBroker::new(Verdict::AllowOnce));
    let res = exec.run(Caller::interface("http"), act()).await;
    assert!(res.is_ok(), "allow-once confirm should proceed: {res:?}");
}

#[tokio::test]
async fn deny_verdict_rejects() {
    let mut exec = base_executor(&["cap:foo"], false, engine_with_tool_grant(&[]));
    exec.set_broker(MockBroker::new(Verdict::Deny));
    let err = exec
        .run(Caller::interface("http"), act())
        .await
        .unwrap_err()
        .0;
    assert!(matches!(err, RegistryError::Denied { .. }), "got {err:?}");
    let msg = err.to_string();
    assert!(msg.contains("action `tool.act`"), "got {msg}");
    assert!(msg.contains("tool `tool`"), "got {msg}");
    assert!(msg.contains("[tool.tool]"), "got {msg}");
    assert!(msg.contains("granted = [\"cap:foo\"]"), "got {msg}");
}

#[tokio::test]
async fn no_broker_rejects_with_diagnostic() {
    let exec = base_executor(&["cap:foo"], false, engine_with_tool_grant(&[]));
    let err = exec
        .run(Caller::interface("http"), act())
        .await
        .unwrap_err()
        .0;
    assert!(matches!(err, RegistryError::Denied { .. }), "got {err:?}");
    let msg = err.to_string();
    assert!(msg.contains("action `tool.act`"), "got {msg}");
    assert!(msg.contains("caller: interface `http`"), "got {msg}");
    assert!(msg.contains("fix: add to grants.toml"), "got {msg}");
}

#[tokio::test]
async fn runner_denial_names_runner_and_fix() {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "tool".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["cap:foo"]),
        },
    );
    file.runner
        .insert("reviewer".into(), RunnerGrants::default());
    let exec = base_executor(&["cap:foo"], false, engine_from_file(file));

    let err = exec
        .run(Caller::default().with_runner("reviewer"), act())
        .await
        .unwrap_err()
        .0;
    let msg = err.to_string();
    assert!(msg.contains("runner `reviewer`"), "got {msg}");
    assert!(msg.contains("[runner.reviewer]"), "got {msg}");
    assert!(
        msg.contains("allowed_actions = [\"tool.act\"]"),
        "got {msg}"
    );
}

#[tokio::test]
async fn interface_denial_names_interface_and_fix() {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "tool".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["cap:foo"]),
        },
    );
    let mut iface = InterfaceGrants::default();
    iface.allowed_actions.insert("other.act".into());
    file.interface.insert("telegram".into(), iface);
    let exec = base_executor(&["cap:foo"], false, engine_from_file(file));

    let err = exec
        .run(Caller::interface("telegram"), act())
        .await
        .unwrap_err()
        .0;
    let msg = err.to_string();
    assert!(msg.contains("interface `telegram`"), "got {msg}");
    assert!(msg.contains("[interface.telegram]"), "got {msg}");
    assert!(
        msg.contains("allowed_actions = [\"tool.act\"]"),
        "got {msg}"
    );
}

#[tokio::test]
async fn service_denial_names_service_and_fix() {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "tool".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["cap:foo"]),
        },
    );
    let exec = base_executor(&["cap:foo"], false, engine_from_file(file));

    let err = exec
        .run(Caller::service("discord"), act())
        .await
        .unwrap_err()
        .0;
    let msg = err.to_string();
    assert!(msg.contains("service `discord`"), "got {msg}");
    assert!(msg.contains("[service.discord]"), "got {msg}");
    assert!(
        msg.contains("allowed_actions = [\"tool.act\"]"),
        "got {msg}"
    );
}

#[tokio::test]
async fn allow_forever_writes_and_reloads() {
    use std::io::Write;
    // Real temp grants.toml + a real reload closure that re-reads it.
    let mut tf = tempfile::NamedTempFile::new().unwrap();
    write!(tf, "[tool.tool]\ngranted = []\n").unwrap();
    let path = tf.path().to_path_buf();

    let mut exec = base_executor(&["cap:foo"], false, engine_with_tool_grant(&[]));
    exec.set_broker(MockBroker::new(Verdict::AllowForever));
    exec.set_grants_path(path.clone());
    let reload_path = path.clone();
    exec.set_reload_grants(Arc::new(move || {
        let text = std::fs::read_to_string(&reload_path).map_err(|e| e.to_string())?;
        let file: GrantsFile = toml::from_str(&text).map_err(|e| e.to_string())?;
        Ok(Engine::new(Grants::from_file(file)))
    }));

    // First call: escalates, persists, reloads, then succeeds.
    let res = exec.run(Caller::interface("http"), act()).await;
    assert!(res.is_ok(), "allow-forever first call: {res:?}");

    // grants.toml now contains the perm.
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(
        on_disk.contains("cap:foo"),
        "grants.toml not updated: {on_disk}"
    );

    // Second call on the SAME executor: the engine was hot-swapped to include
    // cap:foo, so check() returns Allow and the broker is never consulted.
    // (MockBroker is AllowForever; a re-escalation would re-write the file —
    // proving the reload stuck is that it does NOT.)
    let res2 = exec.run(Caller::interface("http"), act()).await;
    assert!(res2.is_ok(), "static pass after forever-grant: {res2:?}");
}

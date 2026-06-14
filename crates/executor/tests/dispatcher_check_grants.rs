//! Verify `Dispatcher::check_grants` on a real `Executor` reflects
//! grants.toml. Doesn't need a live model — it's a permission-engine
//! unit test in disguise, but routed through the public Dispatcher
//! surface that `CodexAppServerProvider` actually uses.

use std::sync::Arc;

use agentd_executor::Executor;
use agentd_permissions::{
    Caller, Engine, Grants, GrantsFile, RunnerGrants, ToolGrants, model::PermissionSet,
};
use agentd_scripting::LuaHost;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{Dispatcher, GrantDecision, Registry};
use async_trait::async_trait;

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

fn build_executor(file: GrantsFile) -> Arc<Executor> {
    let host = LuaHost::new().expect("lua host");
    let skills = host.skills();
    let runners = host.runners();
    let services = host.services();
    let registry: Arc<dyn Registry> = Arc::new(host);
    Arc::new(Executor::new(
        registry,
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        services,
        skills,
        Arc::new(agentd_ai::ProviderRegistry::new()),
    ))
}

#[tokio::test]
async fn check_grants_allows_when_tool_and_runner_grant() {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "codex.shell".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["shell.exec:ls".to_string()]),
        },
    );
    let mut rg = RunnerGrants::default();
    rg.allowed_actions.insert("codex.shell".into());
    file.runner.insert("researcher".into(), rg);

    let exec = build_executor(file);
    let caller = Caller::default().with_runner("researcher");
    let required = PermissionSet::from_iter(["shell.exec:ls".to_string()]);
    let decision = exec.check_grants(caller, "codex.shell", required).await;
    assert!(decision.is_allow(), "expected Allow, got {decision:?}");
}

#[tokio::test]
async fn check_grants_denies_when_tool_missing_perm() {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "codex.shell".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["shell.exec:ls".to_string()]),
        },
    );
    let mut rg = RunnerGrants::default();
    rg.allowed_actions.insert("codex.shell".into());
    file.runner.insert("researcher".into(), rg);

    let exec = build_executor(file);
    let caller = Caller::default().with_runner("researcher");
    // Tool has shell.exec:ls but caller wants shell.exec:rm — should deny.
    let required = PermissionSet::from_iter(["shell.exec:rm".to_string()]);
    let decision = exec.check_grants(caller, "codex.shell", required).await;
    assert!(!decision.is_allow(), "expected Deny, got {decision:?}");
    if let GrantDecision::Deny(reason) = decision {
        assert!(reason.contains("Tool") || reason.contains("missing"));
    }
}

#[tokio::test]
async fn check_grants_denies_when_runner_not_allowlisted() {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "codex.shell".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["shell.exec:*".to_string()]),
        },
    );
    // No runner.allowed_actions for "researcher".
    file.runner
        .insert("researcher".into(), RunnerGrants::default());

    let exec = build_executor(file);
    let caller = Caller::default().with_runner("researcher");
    let required = PermissionSet::from_iter(["shell.exec:ls".to_string()]);
    let decision = exec.check_grants(caller, "codex.shell", required).await;
    assert!(!decision.is_allow(), "expected Deny, got {decision:?}");
}

#[tokio::test]
async fn check_grants_allows_wildcard_grant() {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "codex.shell".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(["shell.exec:*".to_string()]),
        },
    );
    let mut rg = RunnerGrants::default();
    rg.allowed_actions.insert("codex.shell".into());
    file.runner.insert("researcher".into(), rg);

    let exec = build_executor(file);
    let caller = Caller::default().with_runner("researcher");
    let required = PermissionSet::from_iter(["shell.exec:rm".to_string()]);
    let decision = exec.check_grants(caller, "codex.shell", required).await;
    assert!(decision.is_allow(), "wildcard should permit any bin");
}

//! A provider that runs tools of its own reaches files and hosts outside
//! agentd's checks, so a run on it is refused while the operator denies any
//! file or host.

use std::sync::Arc;

use agentd_ai::{CompletionRequest, CompletionResponse, LoopMode, MockProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{
    Caller, Engine, Grants, GrantsFile, Permission, RunnerGrants, model::PermissionSet,
};
use agentd_runners::{RunOptions, RunnerDef, RunnerError, RunnerRegistry};
use agentd_services::ServiceRegistry;
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

/// Answers every request and declares tools of its own, like the Codex
/// providers.
struct OwnTools;
#[async_trait]
impl agentd_ai::Provider for OwnTools {
    fn name(&self) -> &str {
        "own"
    }
    fn loop_mode(&self) -> LoopMode {
        LoopMode::ProviderOwned
    }
    fn has_own_tools(&self) -> bool {
        true
    }
    async fn complete(
        &self,
        _req: CompletionRequest,
    ) -> Result<CompletionResponse, agentd_ai::ProviderError> {
        Ok(MockProvider::text_only("done"))
    }
}

fn executor(denied: &[&str]) -> Arc<Executor> {
    let mut file = GrantsFile::default();
    file.runner.insert(
        "coder".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:own"]),
            ..Default::default()
        },
    );
    for d in denied {
        file.policy.deny_permissions.insert(Permission::new(*d));
    }
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "coder".into(),
        model: Some("own/m".into()),
        ..Default::default()
    });
    let mut providers = ProviderRegistry::new();
    providers.insert("own", Arc::new(OwnTools));
    Arc::new(Executor::new(
        Arc::new(NoActions),
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    ))
}

async fn run(exec: &Arc<Executor>) -> Result<String, RunnerError> {
    exec.run_runner_with_options(
        Caller::interface("ws").with_runner("coder"),
        "coder",
        RunOptions {
            prompt: Some("hi".into()),
            ..Default::default()
        },
        None,
    )
    .await
    .map(|out| out.text)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_denial_refuses_a_run_on_a_provider_with_its_own_tools() {
    let err = run(&executor(&["fs.read:/proj/.env"]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("own"), "names the provider: {err}");
    assert!(
        err.contains("fs.read:/proj/.env"),
        "names the denial: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_denial_refuses_it_too() {
    let err = run(&executor(&["net:tracker.example.com"]))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("net:tracker.example.com"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn other_denials_and_no_denials_leave_it_alone() {
    assert_eq!(run(&executor(&[])).await.unwrap(), "done");
    assert_eq!(run(&executor(&["secret:api_key"])).await.unwrap(), "done");
}

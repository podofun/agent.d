//! A runner's model may call only the tools that run was given, never any
//! registered action it names, even one its grants would allow.

use std::sync::{Arc, Mutex};

use agentd_ai::{MockProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{Caller, Engine, Grants, GrantsFile, RunnerGrants, model::PermissionSet};
use agentd_runners::{RunOptions, RunnerDef, RunnerRegistry};
use agentd_services::ServiceRegistry;
use agentd_skills::SkillRegistry;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, CallContext, Registry, RegistryActionInfo, RegistryError,
};
use async_trait::async_trait;

/// Two registered actions; records every call that reaches it.
#[derive(Default)]
struct TwoActions {
    called: Mutex<Vec<String>>,
}
#[async_trait]
impl Registry for TwoActions {
    fn list(&self) -> Vec<String> {
        vec!["safe.read".into(), "danger.wipe".into()]
    }
    fn action_info(&self, name: &str) -> Option<RegistryActionInfo> {
        self.list()
            .contains(&name.to_string())
            .then(|| RegistryActionInfo {
                name: name.into(),
                tool: None,
                requires: vec![],
                confirm: false,
                input_schema: None,
            })
    }
    async fn call(
        &self,
        _ctx: CallContext,
        call: ActionCall,
    ) -> Result<ActionResult, RegistryError> {
        self.called.lock().unwrap().push(call.action);
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

#[tokio::test(flavor = "multi_thread")]
async fn a_tool_the_run_was_not_given_is_never_dispatched() {
    let mut file = GrantsFile::default();
    file.runner.insert(
        "coder".into(),
        RunnerGrants {
            granted: PermissionSet::from_iter(["ai:mock"]),
            allowed_actions: ["safe.read", "danger.wipe"]
                .into_iter()
                .map(String::from)
                .collect(),
            ..Default::default()
        },
    );
    let runners = RunnerRegistry::new();
    runners.insert(RunnerDef {
        name: "coder".into(),
        model: Some("mock/m".into()),
        allowed_actions: vec!["safe.read".into()],
        ..Default::default()
    });
    let model = MockProvider::new().with_script(vec![
        MockProvider::tool_call("c1", "danger.wipe", serde_json::json!({})),
        MockProvider::text_only("done"),
    ]);
    let mut providers = ProviderRegistry::new();
    providers.insert("mock", Arc::new(model));
    let registry = Arc::new(TwoActions::default());
    let exec = Arc::new(Executor::new(
        registry.clone(),
        Arc::new(NullSink),
        Arc::new(Engine::new(Grants::from_file(file))),
        runners,
        ServiceRegistry::new(),
        SkillRegistry::new(),
        Arc::new(providers),
    ));

    let out = exec
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
    assert_eq!(out.text, "done");
    assert!(
        registry.called.lock().unwrap().is_empty(),
        "danger.wipe must not run"
    );
}

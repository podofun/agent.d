//! Executor preflight (permission check) tests using a fake Registry.

use std::sync::Arc;

use agentd_executor::Executor;
use agentd_permissions::{
    Caller, Engine, Grants, GrantsFile, grants::ToolGrants, model::PermissionSet,
};
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, CallContext, Registry, RegistryActionInfo, RegistryError,
    RegistryToolInfo,
};
use async_trait::async_trait;

/// In-memory registry: action `tool.act` requires `cap:foo`, returns {ok:true}.
struct FakeRegistry {
    action_requires: Vec<String>,
    confirm: bool,
}

#[async_trait]
impl Registry for FakeRegistry {
    fn list(&self) -> Vec<String> {
        vec!["tool.act".into()]
    }
    fn action_info(&self, name: &str) -> Option<RegistryActionInfo> {
        if name != "tool.act" {
            return None;
        }
        Some(RegistryActionInfo {
            name: "tool.act".into(),
            tool: Some("tool".into()),
            requires: self.action_requires.clone(),
            confirm: self.confirm,
            input_schema: None,
        })
    }
    fn tool_info(&self, name: &str) -> Option<RegistryToolInfo> {
        if name != "tool" {
            return None;
        }
        Some(RegistryToolInfo {
            name: "tool".into(),
            requires: Vec::new(),
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

fn engine_with_tool_grant(perms: &[&str]) -> Arc<Engine> {
    let mut file = GrantsFile::default();
    file.tool.insert(
        "tool".into(),
        ToolGrants {
            granted: PermissionSet::from_iter(perms.iter().copied()),
        },
    );
    Arc::new(Engine::new(Grants::from_file(file)))
}

fn executor(requires: &[&str], confirm: bool, engine: Arc<Engine>) -> Executor {
    let reg = Arc::new(FakeRegistry {
        action_requires: requires.iter().map(|s| s.to_string()).collect(),
        confirm,
    });
    Executor::new(
        reg,
        Arc::new(NullSink),
        engine,
        agentd_runners::RunnerRegistry::new(),
        agentd_services::ServiceRegistry::new(),
        agentd_skills::SkillRegistry::new(),
        Arc::new(agentd_ai::ProviderRegistry::new()),
    )
}

#[tokio::test]
async fn allows_when_tool_granted() {
    let exec = executor(&["cap:foo"], false, engine_with_tool_grant(&["cap:foo"]));
    let res = exec
        .run(
            Caller::interface("http"),
            ActionCall {
                action: "tool.act".into(),
                args: serde_json::Value::Null,
            },
        )
        .await;
    assert!(res.is_ok());
}

#[tokio::test]
async fn denies_when_tool_missing_grant() {
    let exec = executor(&["cap:foo"], false, engine_with_tool_grant(&[]));
    let res = exec
        .run(
            Caller::interface("http"),
            ActionCall {
                action: "tool.act".into(),
                args: serde_json::Value::Null,
            },
        )
        .await;
    let err = res.unwrap_err().0;
    assert!(matches!(err, RegistryError::Denied { .. }), "got {err:?}");
    assert!(err.to_string().contains("cap:foo"));
}

#[tokio::test]
async fn needs_confirmation_when_confirm_flag_set() {
    let exec = executor(&["cap:foo"], true, engine_with_tool_grant(&["cap:foo"]));
    let res = exec
        .run(
            Caller::interface("http"),
            ActionCall {
                action: "tool.act".into(),
                args: serde_json::Value::Null,
            },
        )
        .await;
    let err = res.unwrap_err().0;
    assert!(
        matches!(err, RegistryError::NeedsConfirmation(_)),
        "got {err:?}"
    );
}

#[tokio::test]
async fn unknown_action_is_not_found() {
    let exec = executor(&[], false, engine_with_tool_grant(&[]));
    let res = exec
        .run(
            Caller::interface("http"),
            ActionCall {
                action: "nope".into(),
                args: serde_json::Value::Null,
            },
        )
        .await;
    let err = res.unwrap_err().0;
    assert!(matches!(err, RegistryError::NotFound(_)), "got {err:?}");
}

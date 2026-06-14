use agentd_permissions::{Caller, PermissionSet};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod approval;
pub use approval::{ApprovalBroker, ApprovalKind, ApprovalRequest, Verdict};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionCall {
    pub action: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResult {
    pub value: serde_json::Value,
}

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("action `{0}` not registered")]
    NotFound(String),
    #[error("denied at {layer}: {reason}")]
    Denied { layer: String, reason: String },
    #[error("confirmation required: {0}")]
    NeedsConfirmation(String),
    #[error("invocation failed: {0}")]
    Invocation(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryActionInfo {
    pub name: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub confirm: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryToolInfo {
    pub name: String,
    /// What the tool manifest declares it wants. NEVER a grant. The grants file
    /// is the only thing that confers grants.
    #[serde(default)]
    pub requires: Vec<String>,
}

/// Runtime envelope carried with every action invocation. Owns the identity
/// the engine evaluated against, the effective permission set for this
/// execution, and the call chain (deepest-first).
#[derive(Debug, Clone, Default)]
pub struct CallContext {
    pub caller: Caller,
    pub effective_grants: PermissionSet,
    /// Action names in invocation order. `call_chain.first()` is the outermost
    /// action; `call_chain.last()` is the action currently executing.
    pub call_chain: Vec<String>,
}

/// Bridge surface for code that needs to dispatch an action without depending
/// on the `agentd-executor` crate. The executor implements this so loopback
/// MCP servers (and future inbound interfaces) can hand calls back into the
/// permission engine without forming a dependency cycle.
#[async_trait]
pub trait Dispatcher: Send + Sync {
    /// Run a single action with permission engine + trace. Returns the
    /// dispatched `ActionResult` plus the wallclock duration in
    /// milliseconds, or the matching error pair.
    async fn dispatch(
        &self,
        caller: agentd_permissions::Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)>;

    /// Permission-only check. Asks: "if `caller` (typically a runner +
    /// interface) wanted a virtual tool named `tool` whose action requires
    /// `required` permissions, would the engine allow it?". Does NOT
    /// invoke anything; doesn't need the tool to be registered.
    ///
    /// Used by providers that mediate native-host tool calls — codex's
    /// built-in shell, file-write, network — and need to consult the
    /// agentd permission engine before answering codex's per-call
    /// approval request. Default impl returns `Deny` so any Dispatcher
    /// that hasn't opted in fails closed.
    async fn check_grants(
        &self,
        _caller: agentd_permissions::Caller,
        _tool: &str,
        _required: PermissionSet,
    ) -> GrantDecision {
        GrantDecision::Deny("Dispatcher::check_grants not implemented".into())
    }
}

/// Outcome of [`Dispatcher::check_grants`].
#[derive(Debug, Clone)]
pub enum GrantDecision {
    Allow,
    /// `NeedsConfirmation` is collapsed into `Deny` for non-interactive
    /// providers (codex has no human on stdin); we surface the reason
    /// so the caller can log it.
    Deny(String),
}

impl GrantDecision {
    pub fn is_allow(&self) -> bool {
        matches!(self, GrantDecision::Allow)
    }
}

#[async_trait]
pub trait Registry: Send + Sync {
    fn list(&self) -> Vec<String>;
    fn action_info(&self, name: &str) -> Option<RegistryActionInfo> {
        if self.list().iter().any(|n| n == name) {
            Some(RegistryActionInfo {
                name: name.to_string(),
                ..Default::default()
            })
        } else {
            None
        }
    }
    fn tool_info(&self, _name: &str) -> Option<RegistryToolInfo> {
        None
    }
    async fn call(&self, ctx: CallContext, call: ActionCall)
    -> Result<ActionResult, RegistryError>;

    /// Drive a long-running service to completion. The handler lives in the
    /// underlying registry (Lua, native, etc.) keyed by `name`. Returns when
    /// the service body finishes or errors. Default impl reports the service
    /// as not registered — `LuaHost` overrides.
    async fn call_service(&self, _ctx: CallContext, name: &str) -> Result<(), RegistryError> {
        Err(RegistryError::NotFound(format!("service `{name}`")))
    }

    /// Service names registered in this registry. Default = empty.
    fn list_services(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Bridge surface for Lua-side `ctx.run(name, opts)`. The daemon
/// wires the executor in via `LuaHost::set_runner_dispatcher`; without it,
/// the Lua call errors with a clear message.
///
/// JSON in / JSON out keeps the trait dependency-free — scripting never
/// pulls the runners crate in directly.
#[async_trait]
pub trait RunnerDispatcher: Send + Sync {
    /// Dispatch one runner invocation. `opts` mirrors the Lua surface:
    ///
    /// - `prompt`   : string, optional. Becomes the final user message.
    /// - `messages` : `[{role, content}]`, optional. Full conversation.
    /// - `history`  : alias for `messages`.
    /// - `model`    : string, optional. Overrides the runner's default.
    /// - `system`   : string, optional. Appended after the composed system.
    ///
    /// On success returns `{ text, provider, model, stop_reason }`.
    async fn run_runner_json(
        &self,
        caller: agentd_permissions::Caller,
        name: &str,
        opts: serde_json::Value,
    ) -> Result<serde_json::Value, String>;

    /// Names of every registered runner.
    fn runner_names(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Service definition extensions surfaced from Lua via `agentd.service(name, opts, fn)`.
/// Carried separately from the `ServiceDef` in the services crate so types
/// stays at the bottom of the dependency graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceOpts {
    /// Supervision policy. `"always"` restarts on success + crash;
    /// `"on_failure"` restarts only on crash; `"never"` (default) lets the
    /// task exit. Anything else is treated as `"never"` with a warning.
    #[serde(default)]
    pub restart: Option<String>,
    /// Initial backoff before the first restart attempt, in milliseconds.
    /// Doubles on consecutive failures up to `backoff_max_ms`. Default: 1000.
    #[serde(default)]
    pub backoff_ms: Option<u64>,
    /// Cap on the exponential backoff in milliseconds. Default: 60_000.
    #[serde(default)]
    pub backoff_max_ms: Option<u64>,
}

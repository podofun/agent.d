//! Executor — universal scheduler.
//!
//! Owns every registry the daemon spawns work against: actions (`Registry`),
//! runners, services, skills, and providers. Three entry points:
//!
//! - [`Executor::run_action`] — short-lived; permission engine gates, trace
//!   records, handler runs.
//! - [`Executor::run_runner`] — composes a runner's system prompt from its
//!   skills, then drives the tool-use loop: each tool call the model emits is
//!   dispatched back through the action path (so the permission engine fires)
//!   and fed back, until the model returns plain text or the turn ceiling hits.
//! - [`Executor::start_service`] / [`Executor::start_services`] — spawns
//!   long-running services as Tokio tasks, supervising their state in the
//!   service registry.
//!
//! Everything funnels through the same `Registry`-backed dispatch path so the
//! permission engine, the trace sink, and the `ActiveContext` propagation
//! work identically whether the caller is a one-shot HTTP request, an LLM
//! tool call, or a background Discord gateway loop.

mod capture;
mod classifier;
mod runs;

use std::sync::Arc;
use std::time::Instant;

use agentd_ai::ProviderRegistry;
use agentd_permissions::{
    ActionMeta as PermActionMeta, ApprovalSetting, Caller, Decision, Engine, PermissionSet,
    ToolMeta as PermToolMeta,
    engine::{Approved, DenyLayer},
};
use agentd_runners::{CompactPolicy, RunOptions, RunnerError, RunnerOutcome, RunnerRegistry};
use agentd_services::{ServiceRegistry, ServiceState};
use agentd_sessions::{Compaction, Scope, SessionStore};
use agentd_skills::SkillRegistry;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::{
    ActionCall, ActionResult, CallContext, Dispatcher, Registry, RegistryError, RunnerDispatcher,
};
use async_trait::async_trait;
use tokio::task::JoinHandle;

/// Run ids are unique per daemon process.
/// How long a run may take. A chat in a session runs until it finishes or
/// is cancelled unless the caller asks for a deadline; other runs get the
/// default one.
fn run_deadline(options: &RunOptions) -> Option<std::time::Duration> {
    options
        .timeout_ms
        .or(options
            .session_id
            .is_none()
            .then_some(RunOptions::DEFAULT_TIMEOUT_MS))
        .map(std::time::Duration::from_millis)
}

static RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[async_trait]
impl Dispatcher for Executor {
    async fn dispatch(
        &self,
        caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
        self.run(caller, call).await
    }

    async fn check_grants(
        &self,
        caller: Caller,
        tool: &str,
        required: PermissionSet,
    ) -> agentd_types::GrantDecision {
        // Mint a virtual ActionMeta scoped to `tool`. The engine runs
        // the full 5-layer check exactly like it would for any real
        // action: denials, the service and interface allowlists, the runner
        // allowlist, tool package grants cover required, then confirm. The caller (CodexAppServerProvider) passes
        // `tool = "codex"` and `action_name` it picks (e.g.
        // `codex.shell.exec`); user policy decides whether the runner
        // is allowed to use it.
        let tool_meta = PermToolMeta {
            name: tool.to_string(),
            requires: PermissionSet::empty(),
        };
        let action_meta = PermActionMeta {
            name: tool.to_string(),
            tool: Some(tool.to_string()),
            requires: required,
            confirm: false,
        };
        let decision = self
            .engine
            .load()
            .check(&caller, Some(&tool_meta), &action_meta);
        match decision {
            Decision::Allow => agentd_types::GrantDecision::Allow,
            Decision::Deny { .. } | Decision::NeedsConfirmation { .. } => {
                agentd_types::GrantDecision::Deny(
                    decision_to_error(
                        decision,
                        &action_meta,
                        Some(tool),
                        &caller,
                        &self.engine.load(),
                    )
                    .to_string(),
                )
            }
        }
    }
}

/// A classifier refusal: who refused what, then its reason, then the usual
/// diagnostic.
fn classifier_refusal(what: &str, reason: Option<&str>, err: &dyn std::fmt::Display) -> String {
    let reason = reason
        .filter(|r| !r.is_empty())
        .map(|r| format!(" {}", r.trim_end_matches('.').to_owned() + "."))
        .unwrap_or_default();
    format!("The classifier refused `{what}`.{reason}\n{err}")
}

/// Outcome of escalating a denial to the approval broker.
enum Escalation {
    /// Proceed with the dispatch; `extra` carries any allow-once grants.
    Proceed { extra: PermissionSet },
    /// Reject with this error (broker said deny, or persistence failed).
    Reject(RegistryError),
}

/// Reconstruct the `RegistryError` a non-escalated decision produces, with
/// enough component context for users to fix grants.toml without guessing.
fn decision_to_error(
    decision: Decision,
    action_meta: &PermActionMeta,
    tool_name: Option<&str>,
    caller: &Caller,
    engine: &Engine,
) -> RegistryError {
    match decision {
        Decision::NeedsConfirmation { reason } => RegistryError::NeedsConfirmation(format!(
            "{reason} (caller {})\nfix: run `agentctl grants listen` and approve the request, or add `{}` to `[approval].skip` in grants.toml",
            caller_summary(caller),
            action_meta.name
        )),
        Decision::Deny { layer, reason } => RegistryError::Denied {
            layer: deny_layer_label(layer).to_string(),
            reason: denial_reason(layer, reason, action_meta, tool_name, caller, engine),
        },
        Decision::Allow => RegistryError::Invocation(
            "the permission check unexpectedly allowed this action while a denial was being reported — this is a bug in agentd, please report it".into(),
        ),
    }
}

fn denial_reason(
    layer: DenyLayer,
    raw_reason: String,
    action_meta: &PermActionMeta,
    tool_name: Option<&str>,
    caller: &Caller,
    engine: &Engine,
) -> String {
    let action = action_meta.name.as_str();
    let caller_text = caller_summary(caller);
    match layer {
        DenyLayer::Tool => {
            let tool = tool_name
                .or(action_meta.tool.as_deref())
                .unwrap_or("<unknown>");
            let missing = missing_tool_permissions(engine, tool_name, action_meta);
            let missing_text = if missing.is_empty() {
                "the action requirements".to_string()
            } else {
                backtick_list(&missing)
            };
            let fix = if missing.is_empty() {
                format!(
                    "add the required permissions to `{}` in grants.toml",
                    toml_table("tool", tool)
                )
            } else {
                format!(
                    "add to grants.toml:\n{}\ngranted = [{}]",
                    toml_table("tool", tool),
                    toml_array(&missing)
                )
            };
            format!(
                "action `{action}` needs permissions that tool `{tool}` has not been granted\nmissing: {missing_text}\ncaller: {caller_text}\nfix: {fix}"
            )
        }
        DenyLayer::Runner => {
            let runner = caller_runner_name(caller);
            let fix = runner
                .as_deref()
                .map(|runner| {
                    format!(
                        "add to grants.toml:\n{}\nallowed_actions = [{}]",
                        toml_table("runner", runner),
                        toml_array(&[action.to_string()])
                    )
                })
                .unwrap_or_else(|| raw_reason.clone());
            format!(
                "action `{action}` is not allowed for runner `{}`\ncaller: {caller_text}\nfix: {fix}",
                runner.unwrap_or_else(|| "<unknown>".to_string())
            )
        }
        DenyLayer::Interface => {
            let iface = caller
                .interface
                .as_ref()
                .map(|id| id.as_str().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "action `{action}` is not allowed for interface `{iface}`\ncaller: {caller_text}\nfix: add to grants.toml:\n{}\nallowed_actions = [{}]",
                toml_table("interface", &iface),
                toml_array(&[action.to_string()])
            )
        }
        DenyLayer::Service => {
            let service = caller
                .service
                .as_ref()
                .map(|id| id.as_str().to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            format!(
                "action `{action}` is not allowed for service `{service}`\ncaller: {caller_text}\nfix: add to grants.toml:\n{}\nallowed_actions = [{}]",
                toml_table("service", &service),
                toml_array(&[action.to_string()])
            )
        }
        DenyLayer::Denial => {
            let fix = if raw_reason.contains("permission `") {
                "remove the permission from `[denial].permissions` in grants.toml"
            } else {
                "remove the action from `[denial].actions` in grants.toml"
            };
            format!(
                "action `{action}` is denied by the operator\nreason: {raw_reason}\ncaller: {caller_text}\nfix: {fix}"
            )
        }
    }
}

fn missing_tool_permissions(
    engine: &Engine,
    tool_name: Option<&str>,
    action_meta: &PermActionMeta,
) -> Vec<String> {
    let tool_name = tool_name.or(action_meta.tool.as_deref());
    let granted = tool_name
        .and_then(|t| engine.grants().tool(t).map(|g| g.granted.clone()))
        .unwrap_or_else(PermissionSet::empty);
    action_meta
        .requires
        .iter()
        .filter(|p| !granted.contains(p))
        .map(|p| p.as_str().to_string())
        .collect()
}

fn caller_runner_name(caller: &Caller) -> Option<String> {
    caller.runner.as_ref().map(|id| id.as_str().to_string())
}

fn deny_layer_label(layer: DenyLayer) -> &'static str {
    match layer {
        DenyLayer::Denial => "denial",
        DenyLayer::Tool => "tool",
        DenyLayer::Runner => "runner",
        DenyLayer::Interface => "interface",
        DenyLayer::Service => "service",
    }
}

fn caller_summary(caller: &Caller) -> String {
    let mut parts = Vec::new();
    if let Some(v) = &caller.runner {
        parts.push(format!("runner `{}`", v.as_str()));
    }
    if let Some(v) = &caller.interface {
        parts.push(format!("interface `{}`", v.as_str()));
    }
    if let Some(v) = &caller.service {
        parts.push(format!("service `{}`", v.as_str()));
    }
    if let Some(v) = &caller.session {
        parts.push(format!("session `{}`", v.as_str()));
    }
    if let Some(v) = &caller.user {
        parts.push(format!("user `{}`", v.as_str()));
    }
    if parts.is_empty() {
        "direct call".to_string()
    } else {
        parts.join(", ")
    }
}

fn backtick_list(items: &[String]) -> String {
    items
        .iter()
        .map(|s| format!("`{s}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_table(section: &str, name: &str) -> String {
    format!("[{section}.{}]", toml_key(name))
}

fn toml_key(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        toml_string(name)
    }
}

fn toml_array(items: &[String]) -> String {
    items
        .iter()
        .map(|s| toml_string(s))
        .collect::<Vec<_>>()
        .join(", ")
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Coerce a toml_edit item into a string array in place.
fn ensure_string_array(item: &mut toml_edit::Item) -> &mut toml_edit::Array {
    if !item.is_array() {
        *item = toml_edit::value(toml_edit::Array::new());
    }
    item.as_array_mut().expect("ensured array")
}

/// Append values not already present (string compare).
fn append_unique(arr: &mut toml_edit::Array, vals: &[String]) {
    let existing: std::collections::HashSet<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
    for v in vals {
        if !existing.contains(v) {
            arr.push(v.as_str());
        }
    }
}

pub struct Executor {
    registry: Arc<dyn Registry>,
    trace: Arc<dyn TraceSink>,
    /// Held in an `ArcSwap` so an "allow forever" approval can hot-swap a
    /// freshly-reloaded engine without locking the hot `check` path.
    engine: arc_swap::ArcSwap<Engine>,
    runners: RunnerRegistry,
    services: ServiceRegistry,
    skills: SkillRegistry,
    providers: Arc<ProviderRegistry>,
    max_runner_turns: u32,
    runner_slots: tokio::sync::Semaphore,
    /// Optional approval broker. `None` ⇒ escalatable denials reject
    /// immediately (the pre-approvals behavior).
    broker: Option<Arc<dyn agentd_types::ApprovalBroker>>,
    /// Path to `grants.toml`, needed to persist "allow forever" verdicts.
    grants_path: Option<std::path::PathBuf>,
    /// Rebuilds the engine from `grants.toml` (daemon injects the
    /// `load_grants_file → expand_grants → Engine::new` pipeline). Returns the
    /// fresh engine or an error string.
    reload_grants: Option<Arc<dyn Fn() -> Result<Engine, String> + Send + Sync>>,
    /// Monotonic id source for approval requests.
    approval_seq: std::sync::atomic::AtomicU64,
    /// Serializes concurrent "allow forever" read-modify-write + engine swap.
    forever_write_lock: tokio::sync::Mutex<()>,
    /// Durable chat sessions behind `runners.run { session_id }`. `None` ⇒
    /// session runs are rejected with a configuration error.
    sessions: Option<Arc<dyn SessionStore>>,
    /// Session ids with a run in flight; a second run on the same id is
    /// refused rather than interleaved.
    active_sessions: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    /// Records of runs in flight: approved files and hosts, secrets read,
    /// and questions being asked.
    runs: Arc<runs::Runs>,
    /// Questions the classifier answers at once.
    classifier_slots: tokio::sync::Semaphore,
    /// File history and the folder baseline used to capture what each
    /// action call changes. `None` captures nothing.
    capture: Option<capture::Capture>,
    /// The configuration folder (the one holding `init.lua`). A runner's
    /// relative `cwd` resolves against it. It is never a working folder by
    /// itself, so a call with no session folder and no runner `cwd`
    /// captures nothing.
    config_folder: Option<std::path::PathBuf>,
}

/// Removes the session id from the in-flight set when the run ends.
struct SessionGuard {
    id: String,
    set: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

/// The current grants.toml, or an empty document when there is none yet.
/// Any other read failure is an error: treating an unreadable file as empty
/// and writing it back would erase every rule in it, `[denial]` included.
fn read_grants_text(path: &std::path::Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!(
            "could not read grants.toml ({e}) — fix that before approving, or the file would be rewritten from scratch"
        )),
    }
}

/// Replace grants.toml in one step: write a sibling file, flush it, rename
/// it over the original. A crash or a concurrent reload then sees either the
/// old file or the new one, never a torn one.
fn write_grants_text(path: &std::path::Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    let io = |e: std::io::Error| format!("could not write grants.toml ({e}) — check the file permissions");
    let mut file = std::fs::File::create(&tmp).map_err(io)?;
    file.write_all(text.as_bytes()).and_then(|()| file.sync_all()).map_err(io)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

impl Executor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        registry: Arc<dyn Registry>,
        trace: Arc<dyn TraceSink>,
        engine: Arc<Engine>,
        runners: RunnerRegistry,
        services: ServiceRegistry,
        skills: SkillRegistry,
        providers: Arc<ProviderRegistry>,
    ) -> Self {
        Self {
            registry,
            trace,
            engine: arc_swap::ArcSwap::from(engine),
            runners,
            services,
            skills,
            providers,
            max_runner_turns: Self::DEFAULT_MAX_RUNNER_TURNS,
            runner_slots: tokio::sync::Semaphore::new(32),
            broker: None,
            grants_path: None,
            reload_grants: None,
            approval_seq: std::sync::atomic::AtomicU64::new(1),
            forever_write_lock: tokio::sync::Mutex::new(()),
            sessions: None,
            active_sessions: Arc::new(std::sync::Mutex::new(Default::default())),
            runs: Arc::new(runs::Runs::default()),
            classifier_slots: tokio::sync::Semaphore::new(classifier::CLASSIFIER_SLOTS),
            capture: None,
            config_folder: None,
        }
    }

    /// Capture the files every action call changes into `history`, the same
    /// history the `ctx.fs` bindings write, using `baseline` to tell what
    /// changed.
    pub fn set_file_capture(&mut self, history: agentd_fs::History, baseline: agentd_fs::Baseline) {
        self.capture = Some(capture::Capture { history, baseline });
    }

    /// The configuration folder, which a runner's relative `cwd` resolves
    /// against. File changes are never captured in it on its own account.
    pub fn set_config_folder(&mut self, folder: std::path::PathBuf) {
        self.config_folder = Some(folder);
    }

    /// Wire the durable session store. `RedbSessionStore` in production,
    /// `MemSessionStore` in tests.
    pub fn set_sessions(&mut self, store: Arc<dyn SessionStore>) {
        self.sessions = Some(store);
    }

    /// The session store, when one is wired.
    pub fn sessions(&self) -> Option<&Arc<dyn SessionStore>> {
        self.sessions.as_ref()
    }

    /// Wire the approval broker. Without it, escalatable denials reject
    /// immediately.
    pub fn set_broker(&mut self, broker: Arc<dyn agentd_types::ApprovalBroker>) {
        self.broker = Some(broker);
    }

    /// Set the `grants.toml` path used to persist "allow forever" verdicts.
    pub fn set_grants_path(&mut self, path: std::path::PathBuf) {
        self.grants_path = Some(path);
    }

    /// Inject the engine-reload closure (daemon's grants-build pipeline).
    pub fn set_reload_grants(&mut self, f: Arc<dyn Fn() -> Result<Engine, String> + Send + Sync>) {
        self.reload_grants = Some(f);
    }

    /// Override the tool-use loop ceiling (see [`Executor::DEFAULT_MAX_RUNNER_TURNS`]).
    /// The daemon calls this with `runtime.max_turns` from `config.toml`.
    /// A value of 0 is clamped to 1 so a runner always gets at least one turn.
    pub fn set_max_runner_turns(&mut self, turns: u32) {
        self.max_runner_turns = turns.max(1);
    }

    pub fn registry(&self) -> &Arc<dyn Registry> {
        &self.registry
    }
    pub fn runners(&self) -> &RunnerRegistry {
        &self.runners
    }
    pub fn services(&self) -> &ServiceRegistry {
        &self.services
    }
    pub fn skills(&self) -> &SkillRegistry {
        &self.skills
    }
    pub fn providers(&self) -> &Arc<ProviderRegistry> {
        &self.providers
    }

    // ---------- actions ----------

    pub async fn run_action(
        &self,
        caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
        self.run(caller, call).await
    }

    /// Compatibility alias for the previous single-method API.
    pub async fn run(
        &self,
        caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
        self.run_with_changes(caller, call)
            .await
            .map(|(result, duration, _)| (result, duration))
    }

    /// Run an action and report the files it changed, however it changed
    /// them. Each change is also recorded in file history, so it can be
    /// undone.
    pub async fn run_with_changes(
        &self,
        caller: Caller,
        call: ActionCall,
    ) -> Result<(ActionResult, u128, Vec<agentd_ai::FileChange>), (RegistryError, u128)> {
        let action_name = call.action.clone();
        let args = call.args.clone();
        let started = Instant::now();
        let execution = caller.execution_str();

        let action_meta = match self.registry.action_info(&action_name) {
            Some(info) => info_to_perm_action(&info),
            None => {
                let dur = started.elapsed().as_millis();
                let err = RegistryError::NotFound(action_name.clone());
                self.trace
                    .record(
                        TraceEvent::err(&action_name, args, dur, err.to_string())
                            .with_execution(execution.clone())
                            .with_kind("action"),
                    )
                    .await;
                return Err((err, dur));
            }
        };
        let tool_name = action_meta.tool.clone();
        let tool_meta = tool_name
            .as_ref()
            .and_then(|t| self.registry.tool_info(t))
            .map(|i| info_to_perm_tool(&i));

        // Required permissions are the union of the action's own `requires`
        // and its tool's declared `requires`. Fold the tool's into the action
        // meta so the engine check, the escalation request, and grant
        // persistence all operate on the same set.
        let mut action_meta = action_meta;
        if let Some(t) = &tool_meta {
            for p in t.requires.iter() {
                action_meta.requires.insert(p.clone());
            }
        }

        // Check, then ask one question at a time. Every approving answer is
        // recorded and the engine runs again, so an answer never skips a
        // denial or a later question. There are at most three questions
        // (runner allowlist, grants, confirm).
        let mut approved = Approved::default();
        let mut asked = 0;
        loop {
            let decision = self.engine.load().check_approved(
                &caller,
                tool_meta.as_ref(),
                &action_meta,
                &approved,
            );
            if decision == Decision::Allow {
                break;
            }
            let err = if decision.is_escalatable()
                && caller.approval == Some(ApprovalSetting::Nobody)
            {
                // `nobody`: anything not granted is refused, nothing is asked.
                let err = decision_to_error(
                    decision,
                    &action_meta,
                    tool_name.as_deref(),
                    &caller,
                    &self.engine.load(),
                );
                self.trace_nobody(&action_name, &caller).await;
                Some(RegistryError::Denied {
                    layer: "approval".into(),
                    reason: format!(
                        "`{action_name}` was refused without asking, because this run's approval is `nobody`\n{err}"
                    ),
                })
            } else if !decision.is_escalatable()
                || (self.broker.is_none() && caller.approval != Some(ApprovalSetting::Classifier))
            {
                // A denial, or no approver wired: reject with a diagnostic
                // that names the denied component and the grants.toml fix.
                Some(decision_to_error(
                    decision,
                    &action_meta,
                    tool_name.as_deref(),
                    &caller,
                    &self.engine.load(),
                ))
            } else if asked >= 3 {
                Some(RegistryError::Invocation(format!(
                    "the permission check for `{action_name}` asked a fourth question, which should never happen. This is a bug in agentd, please report it"
                )))
            } else {
                asked += 1;
                let runner_question = matches!(
                    decision,
                    Decision::Deny {
                        layer: DenyLayer::Runner,
                        ..
                    }
                );
                let confirm_question = matches!(decision, Decision::NeedsConfirmation { .. });
                match self
                    .escalate(decision, &action_meta, tool_name.as_deref(), &caller, &args)
                    .await
                {
                    Escalation::Proceed { extra } => {
                        for p in extra.iter() {
                            approved.permissions.insert(p.clone());
                        }
                        approved.runner_action |= runner_question;
                        approved.confirm |= confirm_question;
                        None
                    }
                    Escalation::Reject(err) => Some(err),
                }
            };
            if let Some(err) = err {
                let dur = started.elapsed().as_millis();
                self.trace
                    .record(
                        TraceEvent::err(&action_name, args, dur, err.to_string())
                            .with_execution(execution.clone())
                            .with_kind("action"),
                    )
                    .await;
                return Err((err, dur));
            }
        }
        let extra_grants = approved.permissions;

        // Base grants are read AFTER escalation: an "allow forever" verdict has
        // already reloaded the engine, so the missing perm is now static here;
        // an "allow once" verdict instead carries it in `extra_grants`.
        let mut effective_grants = tool_name
            .as_ref()
            .and_then(|t| {
                self.engine
                    .load()
                    .grants()
                    .tool(t)
                    .map(|g| g.granted.clone())
            })
            .unwrap_or_else(PermissionSet::empty);
        for p in extra_grants.iter() {
            effective_grants.insert(p.clone());
        }

        // A call inherits the runner's cwd, else the session's folder (the
        // action's own `cwd`, if any, overrides both registry-side). With
        // neither, the registry falls back to the workspace root.
        let inherited_cwd = caller
            .runner
            .as_ref()
            .and_then(|r| self.runners.get(r.as_str()))
            .and_then(|def| def.cwd)
            .or_else(|| caller.cwd.clone());

        let capturing = self
            .begin_capture(&caller, inherited_cwd.as_deref(), &effective_grants)
            .await;
        let mut call_caller = caller.clone();
        if let Some(capturing) = &capturing {
            capturing.enclose(&mut call_caller);
        }
        let ctx = CallContext {
            caller: call_caller,
            effective_grants,
            call_chain: vec![action_name.clone()],
            cwd: inherited_cwd,
        };

        let outcome = self.registry.call(ctx, call).await;
        let (changes, note) = match (capturing, &self.capture) {
            (Some(capturing), Some(capture)) => {
                let actor = agentd_fs::Actor {
                    action: action_name.clone(),
                    run_id: caller.run_id.clone(),
                };
                capture.finish(capturing, actor).await
            }
            _ => (Vec::new(), None),
        };
        let dur = started.elapsed().as_millis();
        match outcome {
            Ok(res) => {
                self.trace
                    .record(
                        TraceEvent::ok(&action_name, args, dur, res.value.clone())
                            .with_execution(execution.clone())
                            .with_kind("action")
                            .with_note(note),
                    )
                    .await;
                Ok((res, dur, changes))
            }
            Err(e) => {
                self.trace
                    .record(
                        TraceEvent::err(&action_name, args, dur, e.to_string())
                            .with_execution(execution.clone())
                            .with_kind("action")
                            .with_note(note),
                    )
                    .await;
                Err((e, dur))
            }
        }
    }

    /// Start capturing the folders this call may change. Nothing is captured
    /// when no capture is set up, the call has no working folder (a session
    /// folder or a runner `cwd`), or it has no `fs.write` grant there. A
    /// nested call never waits for a folder: it leaves its enclosing call's
    /// folders to that call, and only scans them itself when the enclosing
    /// call does not scan and this one may run programs.
    async fn begin_capture(
        &self,
        caller: &Caller,
        inherited_cwd: Option<&str>,
        grants: &PermissionSet,
    ) -> Option<capture::Started> {
        let capture = self.capture.as_ref()?;
        let working = match inherited_cwd.map(std::path::Path::new)? {
            cwd if cwd.is_absolute() => cwd.to_path_buf(),
            cwd => self.config_folder.as_ref()?.join(cwd),
        };
        let working = capture::canonical(&working);
        let grants = capture::with_cwd(grants, caller.cwd.as_deref());
        let roots = capture::capture_roots(&working, &grants);
        let scans = capture::runs_programs(caller, &grants);
        if caller.capturing.is_empty() {
            if roots.is_empty() {
                return None;
            }
            return Some(capture.begin(working, roots, scans, true).await);
        }
        if caller.capturing_scans || !scans {
            return None;
        }
        let held: Vec<_> = roots
            .into_iter()
            .filter(|root| caller.capturing.iter().any(|h| root.starts_with(h)))
            .collect();
        if held.is_empty() {
            return None;
        }
        Some(capture.begin(working, held, true, false).await)
    }

    /// `changes` with every secret the run read replaced by its name in each
    /// diff, as tool inputs and outputs are.
    fn redact_changes(
        &self,
        run_id: Option<&str>,
        changes: &[agentd_ai::FileChange],
    ) -> Vec<agentd_ai::FileChange> {
        changes
            .iter()
            .cloned()
            .map(|mut change| {
                if let Some(diff) = change.diff.take() {
                    let redacted = self
                        .runs
                        .redact_secrets(run_id, &serde_json::Value::String(diff));
                    change.diff = redacted.as_str().map(str::to_owned);
                }
                change
            })
            .collect()
    }

    // ---------- approval escalation ----------

    /// Answer one question: the classifier when the run's approval is
    /// `classifier` and it may answer this kind, otherwise (or when it is
    /// unsure) a human. Identical questions in flight share the answer. The
    /// classifier never allows forever.
    async fn decide(
        &self,
        mut req: agentd_types::ApprovalRequest,
        key: Option<String>,
    ) -> runs::Answered {
        self.runs
            .ask(key, async {
                let mut unsure = None;
                if self.classifier_answers(&req) {
                    match self.classify(&req).await {
                        classifier::ClassifierAnswer::Allow(reason) => {
                            return runs::Answered {
                                verdict: agentd_types::Verdict::AllowOnce,
                                decided_by: "classifier",
                                reason: Some(reason),
                            };
                        }
                        classifier::ClassifierAnswer::Refuse(reason) => {
                            return runs::Answered {
                                verdict: agentd_types::Verdict::Deny,
                                decided_by: "classifier",
                                reason: Some(reason),
                            };
                        }
                        classifier::ClassifierAnswer::Unsure(reason) => {
                            // The person who answers sees why it came to them.
                            req.reason = format!("The classifier was unsure: {reason}");
                            unsure = Some(format!("the classifier was unsure: {reason}"));
                        }
                    }
                }
                let verdict = match &self.broker {
                    Some(broker) => broker.request(req).await,
                    None => agentd_types::Verdict::Deny,
                };
                runs::Answered {
                    verdict,
                    decided_by: "human",
                    reason: unsure,
                }
            })
            .await
    }

    /// Record a question refused without asking because the run's approval
    /// is `nobody`.
    async fn trace_nobody(&self, action: &str, caller: &Caller) {
        self.trace
            .record(
                TraceEvent::decision(
                    action,
                    serde_json::json!({
                        "run_id": caller.run_id,
                        "session_id": caller.session_id,
                    }),
                    serde_json::json!({ "approval": "Deny", "decided_by": "nobody" }),
                )
                .with_execution(caller.execution_str()),
            )
            .await;
    }

    /// Ask about a non-`Allow`, escalatable decision (the classifier or a
    /// human, see [`Executor::decide`]) and translate the answer into an
    /// [`Escalation`].
    async fn escalate(
        &self,
        decision: Decision,
        action_meta: &PermActionMeta,
        tool_name: Option<&str>,
        caller: &Caller,
        args: &serde_json::Value,
    ) -> Escalation {
        use std::sync::atomic::Ordering;

        // Classify + compute the missing perms (Tool layer) using the same
        // wildcard-aware `contains` the engine uses, so we persist exactly what
        // the engine would require.
        let (kind, missing): (agentd_types::ApprovalKind, Vec<String>) = match &decision {
            Decision::NeedsConfirmation { .. } => (agentd_types::ApprovalKind::Confirm, Vec::new()),
            Decision::Deny {
                layer: DenyLayer::Runner,
                ..
            } => (agentd_types::ApprovalKind::RunnerAction, Vec::new()),
            Decision::Deny { .. } => {
                let pkg_granted = tool_name
                    .and_then(|t| {
                        self.engine
                            .load()
                            .grants()
                            .tool(t)
                            .map(|g| g.granted.clone())
                    })
                    .unwrap_or_else(PermissionSet::empty);
                let missing = action_meta
                    .requires
                    .iter()
                    .filter(|r| !pkg_granted.contains(r))
                    .map(|p| p.as_str().to_string())
                    .collect();
                (agentd_types::ApprovalKind::MissingGrant, missing)
            }
            Decision::Allow => unreachable!("escalate called on Allow"),
        };
        let reason = match &decision {
            Decision::NeedsConfirmation { reason } => reason.clone(),
            Decision::Deny { reason, .. } => reason.clone(),
            Decision::Allow => String::new(),
        };

        let id = self.approval_seq.fetch_add(1, Ordering::Relaxed);
        let req = agentd_types::ApprovalRequest {
            id,
            kind,
            action: action_meta.name.clone(),
            tool: tool_name.map(|s| s.to_string()),
            requires: action_meta
                .requires
                .iter()
                .map(|p| p.as_str().to_string())
                .collect(),
            missing: missing.clone(),
            reason,
            caller: caller.clone(),
            detail: Some(self.runs.redact(caller.run_id.as_deref(), args)),
            run_id: caller.run_id.clone(),
            session_id: caller.session_id.clone(),
            warning: None,
        };
        let key = caller
            .run_id
            .as_ref()
            .map(|r| format!("{r}\0{kind:?}\0{}\0{args}", action_meta.name));
        let detail = req.detail.clone();
        let answered = self.decide(req, key).await;
        let verdict = answered.verdict;
        self.trace
            .record(
                TraceEvent::decision(
                    &action_meta.name,
                    serde_json::json!({
                        "request_id": id,
                        "run_id": caller.run_id,
                        "session_id": caller.session_id,
                        "detail": detail,
                    }),
                    serde_json::json!({
                        "approval": format!("{verdict:?}"),
                        "decided_by": answered.decided_by,
                        "reason": answered.reason,
                    }),
                )
                .with_execution(caller.execution_str()),
            )
            .await;

        match verdict {
            agentd_types::Verdict::Deny => {
                let err = decision_to_error(
                    decision,
                    action_meta,
                    tool_name,
                    caller,
                    &self.engine.load(),
                );
                Escalation::Reject(match (answered.decided_by, answered.reason) {
                    ("classifier", reason) => RegistryError::Denied {
                        layer: "approval".into(),
                        reason: classifier_refusal(&action_meta.name, reason.as_deref(), &err),
                    },
                    _ => err,
                })
            }
            agentd_types::Verdict::AllowOnce => Escalation::Proceed {
                extra: PermissionSet::from_iter(missing),
            },
            agentd_types::Verdict::AllowForever => {
                // Persist + reload. If the persistence plumbing is absent,
                // degrade to allow-once rather than silently dropping the
                // approval. A genuine write/reload failure rejects.
                if self.grants_path.is_none() || self.reload_grants.is_none() {
                    tracing::warn!(
                        action = %action_meta.name,
                        "allow-forever requested but grants path/reload not wired; \
                         degrading to allow-once"
                    );
                    return Escalation::Proceed {
                        extra: PermissionSet::from_iter(missing),
                    };
                }
                match self
                    .apply_forever(
                        kind,
                        tool_name,
                        caller.runner.as_ref().map(|r| r.as_str()),
                        &action_meta.name,
                        &missing,
                    )
                    .await
                {
                    Ok(()) => Escalation::Proceed {
                        extra: PermissionSet::from_iter(missing),
                    },
                    Err(e) => Escalation::Reject(RegistryError::Invocation(format!(
                        "the approval was granted but could not be saved to grants.toml ({e}) — approve again or edit grants.toml by hand"
                    ))),
                }
            }
        }
    }

    /// Remember `folder` (an absolute real path) as trusted: append it to
    /// `[folders].trusted` in grants.toml, once, and reload.
    pub async fn trust_folder(&self, folder: &str) -> Result<(), String> {
        self.apply_forever(agentd_types::ApprovalKind::Folder, None, None, folder, &[])
            .await
    }

    /// Ask a person whether a chat session may work in `folder`. The run's
    /// approval setting never applies: the classifier does not answer folder
    /// questions, and with nobody to ask the answer is no.
    pub async fn ask_folder(&self, folder: &str, caller: &Caller) -> agentd_types::Verdict {
        let id = self
            .approval_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let request = agentd_types::ApprovalRequest {
            id,
            kind: agentd_types::ApprovalKind::Folder,
            action: folder.to_owned(),
            tool: None,
            requires: Vec::new(),
            missing: Vec::new(),
            reason: "Let agent.d work in this folder?".to_owned(),
            caller: caller.clone(),
            detail: Some(serde_json::json!({ "folder": folder })),
            run_id: None,
            session_id: None,
            warning: None,
        };
        let verdict = match &self.broker {
            Some(broker) => broker.request(request).await,
            None => agentd_types::Verdict::Deny,
        };
        self.trace
            .record(TraceEvent::decision(
                "folder",
                serde_json::json!({ "request_id": id, "folder": folder, "caller": caller }),
                serde_json::json!({ "approval": format!("{verdict:?}"), "decided_by": "human" }),
            ))
            .await;
        verdict
    }

    /// Whether the user trusts `folder` (an absolute real path).
    pub fn trusts_folder(&self, folder: &std::path::Path) -> bool {
        self.engine.load().grants().trusts_folder(folder)
    }

    /// Persist an "allow forever" verdict to `grants.toml` and hot-reload the
    /// engine. Serialized against concurrent writers via `forever_write_lock`.
    async fn apply_forever(
        &self,
        kind: agentd_types::ApprovalKind,
        tool_name: Option<&str>,
        runner_name: Option<&str>,
        action_name: &str,
        missing: &[String],
    ) -> Result<(), String> {
        let path = self.grants_path.as_ref().ok_or_else(|| {
            "the daemon has no grants file path configured, so approvals cannot be saved"
                .to_string()
        })?;
        let reload = self.reload_grants.as_ref().ok_or_else(|| {
            "the daemon has no grants reload hook wired, so approvals cannot be saved".to_string()
        })?;

        let _guard = self.forever_write_lock.lock().await;

        let text = read_grants_text(path)?;
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| {
            format!("grants.toml is not valid TOML ({e}) — fix the syntax and approve again")
        })?;

        match kind {
            agentd_types::ApprovalKind::MissingGrant => {
                let tool =
                    tool_name.ok_or_else(|| {
                        "the approval did not name a tool, so there is nothing to grant the permissions to".to_string()
                    })?;
                let granted = doc
                    .as_table_mut()
                    .entry("tool")
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("the `tool` key in grants.toml is not a table — remove or rename the conflicting `tool` entry")?
                    .entry(tool)
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("this tool's entry in grants.toml is not a table — remove or rename the conflicting entry")?
                    .entry("granted")
                    .or_insert(toml_edit::value(toml_edit::Array::new()));
                append_unique(ensure_string_array(granted), missing);
            }
            agentd_types::ApprovalKind::Confirm => {
                let ac = doc
                    .as_table_mut()
                    .entry("approval")
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("the `approval` key in grants.toml is not a table. Remove or rename the conflicting `approval` entry")?
                    .entry("skip")
                    .or_insert(toml_edit::value(toml_edit::Array::new()));
                append_unique(
                    ensure_string_array(ac),
                    std::slice::from_ref(&action_name.to_string()),
                );
            }
            agentd_types::ApprovalKind::RunnerAction => {
                let runner = runner_name.ok_or_else(|| {
                    "the approval did not name a runner, so there is no `allowed_actions` list to extend".to_string()
                })?;
                let allowed = doc
                    .as_table_mut()
                    .entry("runner")
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("the `runner` key in grants.toml is not a table — remove or rename the conflicting `runner` entry")?
                    .entry(runner)
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("this runner's entry in grants.toml is not a table — remove or rename the conflicting entry")?
                    .entry("allowed_actions")
                    .or_insert(toml_edit::value(toml_edit::Array::new()));
                append_unique(
                    ensure_string_array(allowed),
                    std::slice::from_ref(&action_name.to_string()),
                );
            }
            agentd_types::ApprovalKind::Folder => {
                let trusted = doc
                    .as_table_mut()
                    .entry("folders")
                    .or_insert(toml_edit::table())
                    .as_table_mut()
                    .ok_or("the `folders` key in grants.toml is not a table. Remove or rename the conflicting `folders` entry")?
                    .entry("trusted")
                    .or_insert(toml_edit::value(toml_edit::Array::new()));
                append_unique(
                    ensure_string_array(trusted),
                    std::slice::from_ref(&action_name.to_string()),
                );
            }
        }

        write_grants_text(path, &doc.to_string())?;
        let fresh = reload()?;
        self.engine.store(Arc::new(fresh));
        Ok(())
    }

    /// Persist an inline `AllowForever` verdict: append `perm` to the
    /// `granted` list of the `[tool.<name>]` / `[service.<name>]` entry that
    /// owns the executing handler, then hot-reload the engine.
    async fn persist_inline_grant(
        &self,
        grant_kind: Option<&str>,
        grant_name: Option<&str>,
        perm: &str,
    ) -> Result<(), String> {
        let (kind, name) = match (grant_kind, grant_name) {
            (Some(k @ ("tool" | "service")), Some(n)) => (k, n),
            _ => {
                return Err(
                    "the execution has no tool or service grants entry to extend".to_string(),
                );
            }
        };
        let path = self.grants_path.as_ref().ok_or_else(|| {
            "the daemon has no grants file path configured, so approvals cannot be saved"
                .to_string()
        })?;
        let reload = self.reload_grants.as_ref().ok_or_else(|| {
            "the daemon has no grants reload hook wired, so approvals cannot be saved".to_string()
        })?;

        let _guard = self.forever_write_lock.lock().await;

        let text = read_grants_text(path)?;
        let mut doc = text.parse::<toml_edit::DocumentMut>().map_err(|e| {
            format!("grants.toml is not valid TOML ({e}) — fix the syntax and approve again")
        })?;
        let granted = doc
            .as_table_mut()
            .entry(kind)
            .or_insert(toml_edit::table())
            .as_table_mut()
            .ok_or_else(|| format!("the `{kind}` key in grants.toml is not a table — remove or rename the conflicting entry"))?
            .entry(name)
            .or_insert(toml_edit::table())
            .as_table_mut()
            .ok_or_else(|| format!("the `[{kind}.{name}]` entry in grants.toml is not a table — remove or rename the conflicting entry"))?
            .entry("granted")
            .or_insert(toml_edit::value(toml_edit::Array::new()));
        append_unique(
            ensure_string_array(granted),
            std::slice::from_ref(&perm.to_string()),
        );

        write_grants_text(path, &doc.to_string())?;
        let fresh = reload()?;
        self.engine.store(Arc::new(fresh));
        Ok(())
    }

    // ---------- runners ----------

    /// Default ceiling on tool-use loop iterations for runs outside a session.
    /// A misbehaving model that keeps emitting tool calls without ever
    /// returning text would otherwise run forever. Session runs have no
    /// ceiling. Override the active ceiling via [`Executor::set_max_runner_turns`].
    pub const DEFAULT_MAX_RUNNER_TURNS: u32 = 16;

    /// Run a runner. For `LoopMode::ExecutorOwned` providers (Claude API,
    /// OpenAI API, Mock) this owns the agent loop:
    ///
    /// 1. Compose system prompt + tool catalog from the runner's skills and
    ///    `allowed_actions`.
    /// 2. Call `provider.complete(req)`.
    /// 3. If the response carries `tool_calls`, dispatch each through
    ///    `Executor::run` so the full permission engine fires, append a
    ///    `Role::Tool` message per call, and loop.
    /// 4. Stop when the provider returns a text-only reply or, outside a
    ///    session, the `max_turns` budget is exhausted.
    ///
    /// For `LoopMode::ProviderOwned` providers (Claude CLI w/ MCP, Codex CLI
    /// w/ MCP) the executor just configures `mcp_endpoint` and calls
    /// `complete` once — the provider already runs its own loop.
    pub async fn run_runner(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        prompt: String,
    ) -> Result<RunnerOutcome, RunnerError> {
        self.run_runner_extended(caller, runner_name, Some(prompt), None, None, None, None)
            .await
    }

    /// Like [`Executor::run_runner`], additionally pushing
    /// [`agentd_ai::StreamEvent`]s into `sink` as the provider produces
    /// output. The complete [`RunnerOutcome`] is still returned — streaming
    /// is a live view, never a replacement for the full response.
    pub async fn run_runner_streaming(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        prompt: String,
        sink: agentd_ai::StreamSink,
    ) -> Result<RunnerOutcome, RunnerError> {
        self.run_runner_extended(
            caller,
            runner_name,
            Some(prompt),
            None,
            None,
            None,
            Some(sink),
        )
        .await
    }

    /// Translate runner.allowed_actions into `ToolDef`s the provider sees.
    /// Pulls each action's description + required-perms summary from the
    /// registry; unknown action names are silently skipped (don't expose a
    /// non-existent tool to the model).
    fn build_tool_catalog(&self, allowed: &[String]) -> Vec<agentd_ai::ToolDef> {
        let mut out = Vec::new();
        for name in allowed {
            let Some(info) = self.registry.action_info(name) else {
                continue;
            };
            let description = if info.requires.is_empty() {
                None
            } else {
                Some(format!("Requires: {}", info.requires.join(", ")))
            };
            out.push(agentd_ai::ToolDef {
                name: info.name,
                description,
                // Prefer the action's declared schema (compiled from its Lua
                // `input` table) so the model sees exact field names, types,
                // and required-ness. Actions without one fall back to a
                // free-form unconstrained object.
                input_schema: info
                    .input_schema
                    .unwrap_or_else(|| serde_json::json!({ "type": "object" })),
            });
        }
        out
    }

    /// Spawn one service as a Tokio task and return its `JoinHandle`. The
    /// task drives the Lua body to completion (or until daemon shutdown).
    /// Honors the service's `restart` / `backoff_ms` policy: `"always"` and
    /// `"on_failure"` re-enter the body after exponential backoff; otherwise
    /// the task exits on the first clean return or error.
    pub fn start_service(&self, name: &str) -> Option<JoinHandle<()>> {
        let def = match self.services.get(name) {
            Some(d) => d,
            None => {
                tracing::warn!(service = name, "start_service: unknown service");
                return None;
            }
        };

        let registry = self.registry.clone();
        let services = self.services.clone();
        let trace = self.trace.clone();
        let granted = self
            .engine
            .load()
            .grants()
            .service(name)
            .map(|g| g.granted.clone())
            .unwrap_or_else(PermissionSet::empty);
        let svc_name = name.to_string();
        let restart = def.restart.clone();
        let backoff_ms = def.backoff_ms.unwrap_or(1000);
        let backoff_max_ms = def.backoff_max_ms.unwrap_or(60_000);

        services.set_state(&svc_name, ServiceState::Running);
        trace_lifecycle(&trace, &svc_name, "started").await_blocking();

        let handle = tokio::spawn(async move {
            let mut current_backoff = backoff_ms;
            loop {
                let ctx = CallContext {
                    caller: Caller::service(svc_name.clone()),
                    effective_grants: granted.clone(),
                    call_chain: vec![format!("service:{svc_name}")],
                    cwd: None,
                };
                let outcome = registry.call_service(ctx, &svc_name).await;
                let crashed = outcome.is_err();
                match &outcome {
                    Ok(()) => {
                        services.set_state(&svc_name, ServiceState::Stopped);
                        trace
                            .record(TraceEvent::ok(
                                &format!("service:{svc_name}"),
                                serde_json::Value::Null,
                                0,
                                serde_json::json!({ "lifecycle": "stopped" }),
                            ))
                            .await;
                        tracing::info!(service = %svc_name, "service stopped");
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        services.set_error(&svc_name, &msg);
                        trace
                            .record(TraceEvent::err(
                                &format!("service:{svc_name}"),
                                serde_json::Value::Null,
                                0,
                                msg.clone(),
                            ))
                            .await;
                        tracing::error!(service = %svc_name, error = %msg, "service crashed");
                    }
                }
                let should_restart = match restart.as_deref() {
                    Some("always") => true,
                    Some("on_failure") => crashed,
                    _ => false,
                };
                if !should_restart {
                    break;
                }
                if crashed {
                    tracing::warn!(
                        service = %svc_name, backoff_ms = current_backoff,
                        "supervised restart in {current_backoff}ms"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(current_backoff)).await;
                    current_backoff = (current_backoff.saturating_mul(2)).min(backoff_max_ms);
                } else {
                    // Clean exits reset backoff so a stable service that briefly
                    // returns doesn't get throttled.
                    current_backoff = backoff_ms;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                services.set_state(&svc_name, ServiceState::Running);
            }
        });
        Some(handle)
    }

    /// Spawn every registered service. Returns the join handles in name order
    /// so the daemon can await or supervise them.
    pub fn start_services(&self) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        for name in self.services.names() {
            if let Some(h) = self.start_service(&name) {
                handles.push(h);
            }
        }
        handles
    }
}

// Tiny helper so the synchronous `start_service` body can still emit a
// lifecycle trace event without forcing the caller into async.
struct DeferredTrace<'a> {
    trace: &'a Arc<dyn TraceSink>,
    name: String,
    phase: &'static str,
}

impl DeferredTrace<'_> {
    fn await_blocking(self) {
        // Fire-and-forget: spawn the trace write so we don't block the caller.
        let trace = self.trace.clone();
        let name = self.name;
        let phase = self.phase;
        tokio::spawn(async move {
            trace
                .record(TraceEvent::ok(
                    &format!("service:{name}"),
                    serde_json::Value::Null,
                    0,
                    serde_json::json!({ "lifecycle": phase }),
                ))
                .await;
        });
    }
}

fn trace_lifecycle<'a>(
    trace: &'a Arc<dyn TraceSink>,
    name: &str,
    phase: &'static str,
) -> DeferredTrace<'a> {
    DeferredTrace {
        trace,
        name: name.to_string(),
        phase,
    }
}

fn info_to_perm_action(info: &agentd_types::RegistryActionInfo) -> PermActionMeta {
    PermActionMeta {
        name: info.name.clone(),
        tool: info.tool.clone(),
        requires: PermissionSet::from_iter(info.requires.iter().cloned()),
        confirm: info.confirm,
    }
}

fn info_to_perm_tool(info: &agentd_types::RegistryToolInfo) -> PermToolMeta {
    PermToolMeta {
        name: info.name.clone(),
        requires: PermissionSet::from_iter(info.requires.iter().cloned()),
    }
}

// ---------- RunnerDispatcher impl ----------
//
// Bridges Lua's `agentd.runners.run(name, opts)` into the executor's
// `run_runner_extended` pipeline. Opts come in as JSON:
//
//   { prompt?, messages?, history?, system?, model? }
//
// The dispatcher is a thin wrapper around `Arc<Executor>` because the
// underlying runner loop needs to clone `Arc<Self>` for the MCP loopback
// branch — a trait method can't bound that on a raw `&Executor` ref.

pub struct ExecutorHandle(pub Arc<Executor>);

impl ExecutorHandle {
    pub fn new(executor: Arc<Executor>) -> Arc<Self> {
        Arc::new(Self(executor))
    }
}

/// Bridge for inline (mid-handler) permission escalations from the scripting
/// scheduler. Reuses the same broker, trace kind, and grants persistence the
/// dispatch-time escalation path uses, so the operator experience is one flow.
/// Denials are read from the engine at the moment of each check, so a reload
/// (a `--watch` rebuild, or the reload after an "allow forever") applies to
/// the very next call.
impl agentd_types::Denials for Executor {
    fn denied_permissions(&self) -> agentd_permissions::PermissionSet {
        self.engine.load().grants().denial().permissions.clone()
    }

    fn denies_action(&self, name: &str) -> bool {
        self.engine.load().grants().denial().denies_action(name)
    }
}

#[async_trait]
impl agentd_types::InlineApprovals for Executor {
    async fn request_inline(
        &self,
        req: agentd_types::InlineApprovalRequest,
    ) -> agentd_types::Verdict {
        self.request_inline_explained(req).await.verdict
    }

    async fn request_inline_explained(
        &self,
        req: agentd_types::InlineApprovalRequest,
    ) -> agentd_types::InlineAnswer {
        use std::sync::atomic::Ordering;
        let refused = |refusal: Option<String>| agentd_types::InlineAnswer {
            verdict: agentd_types::Verdict::Deny,
            refusal,
        };

        if req.caller.approval == Some(ApprovalSetting::Nobody) {
            let action = req.call_chain.first().map_or("<lua>", String::as_str);
            self.trace_nobody(action, &req.caller).await;
            return refused(Some(format!(
                "`{}` was refused without asking, because this run's approval is `nobody`.",
                req.permission
            )));
        }
        if self.broker.is_none() && req.caller.approval != Some(ApprovalSetting::Classifier) {
            return refused(None);
        }
        let id = self.approval_seq.fetch_add(1, Ordering::Relaxed);
        let action = req
            .call_chain
            .first()
            .cloned()
            .unwrap_or_else(|| "<lua>".to_string());
        let subject = match (req.grant_kind.as_deref(), req.grant_name.as_deref()) {
            (Some(k), Some(n)) => format!("{k} `{n}`"),
            _ => "the executing script".to_string(),
        };
        let approval = agentd_types::ApprovalRequest {
            id,
            kind: agentd_types::ApprovalKind::MissingGrant,
            action: action.clone(),
            tool: req.grant_name.clone(),
            requires: vec![req.permission.clone()],
            missing: vec![req.permission.clone()],
            reason: format!(
                "the handler asked for `{}` mid-run, which {subject} has not been granted",
                req.permission
            ),
            caller: req.caller.clone(),
            detail: req
                .detail
                .as_ref()
                .map(|d| self.runs.redact(req.caller.run_id.as_deref(), d)),
            run_id: req.caller.run_id.clone(),
            session_id: req.caller.session_id.clone(),
            warning: runs::interpreter_warning(&req.permission),
        };
        // Identical questions are matched on the unredacted request.
        let key = req.caller.run_id.as_ref().map(|r| {
            format!(
                "{r}\0inline\0{}\0{}",
                req.permission,
                req.detail
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default()
            )
        });
        let detail = approval.detail.clone();
        let answered = self.decide(approval, key).await;
        let verdict = answered.verdict;
        self.trace
            .record(
                TraceEvent::decision(
                    &action,
                    serde_json::json!({
                        "request_id": id,
                        "permission": req.permission,
                        "run_id": req.caller.run_id,
                        "session_id": req.caller.session_id,
                        "detail": detail,
                    }),
                    serde_json::json!({
                        "approval": format!("{verdict:?}"),
                        "decided_by": answered.decided_by,
                        "reason": answered.reason,
                    }),
                )
                .with_execution(req.caller.execution_str()),
            )
            .await;
        if verdict == agentd_types::Verdict::Deny {
            return refused((answered.decided_by == "classifier").then(|| {
                classifier_refusal(&req.permission, answered.reason.as_deref(), &"")
                    .trim_end()
                    .to_string()
            }));
        }
        if let Some(run_id) = req.caller.run_id.as_deref() {
            self.runs.allow(run_id, &req.permission);
        }
        if verdict == agentd_types::Verdict::AllowForever
            && let Err(e) = self
                .persist_inline_grant(
                    req.grant_kind.as_deref(),
                    req.grant_name.as_deref(),
                    &req.permission,
                )
                .await
        {
            tracing::warn!(
                permission = %req.permission,
                "allow-forever could not be saved ({e}); degrading to allow-once"
            );
            return agentd_types::InlineAnswer {
                verdict: agentd_types::Verdict::AllowOnce,
                refusal: None,
            };
        }
        agentd_types::InlineAnswer {
            verdict,
            refusal: None,
        }
    }

    fn classifier_configured(&self) -> bool {
        self.classifier_name().is_some()
    }

    fn run_allows(&self, run_id: &str, permission: &str) -> bool {
        self.runs.allows(run_id, permission)
    }

    fn record_secret(&self, run_id: &str, name: &str, value: &str) {
        self.runs.record_secret(run_id, name, value);
    }
}

#[async_trait]
impl RunnerDispatcher for ExecutorHandle {
    async fn run_runner_json(
        &self,
        caller: Caller,
        name: &str,
        opts: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let options: RunOptions =
            serde_json::from_value(opts).map_err(|e| format!("invalid runner options: {e}"))?;

        let out = self
            .0
            .run_runner_with_options(caller, name, options, None)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(out).map_err(|e| e.to_string())
    }

    fn runner_names(&self) -> Vec<String> {
        self.0.runners.names()
    }
}

impl Executor {
    /// Run a runner with the full set of overrides Lua's
    /// `agentd.runners.run` accepts. Equivalent to `run_runner` for the
    /// simple-prompt case; extends it with explicit `messages` and per-call
    /// `system` / `model` overrides.
    // Retained for Rust callers of the earlier argument-based API.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_runner_extended(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        prompt: Option<String>,
        messages: Option<serde_json::Value>,
        system_override: Option<String>,
        model_override: Option<String>,
        sink: Option<agentd_ai::StreamSink>,
    ) -> Result<RunnerOutcome, RunnerError> {
        let options = RunOptions {
            prompt,
            messages: messages.map(parse_messages).transpose()?,
            system: system_override,
            model: model_override,
            ..Default::default()
        };
        self.run_runner_with_options(caller, runner_name, options, sink)
            .await
    }

    /// Validate and bound every runner entry point, including nested Lua calls.
    pub async fn run_runner_with_options(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        options: RunOptions,
        sink: Option<agentd_ai::StreamSink>,
    ) -> Result<RunnerOutcome, RunnerError> {
        options.validate()?;
        let _slot = self
            .runner_slots
            .try_acquire()
            .map_err(|_| RunnerError::Busy)?;
        let deadline = run_deadline(&options);
        let started = Instant::now();
        let execution = caller.execution_str();
        let run = self.run_runner_inner(caller, runner_name, options, sink.clone());
        let mut result = match deadline {
            Some(deadline) => tokio::time::timeout(deadline, run)
                .await
                .unwrap_or(Err(RunnerError::Timeout)),
            None => run.await,
        };
        if sink.as_ref().is_some_and(|s| s.overflowed()) {
            result = Err(RunnerError::SlowConsumer);
        }
        let action = format!("runner:{runner_name}");
        let args = serde_json::json!({ "runner": runner_name });
        let duration = started.elapsed().as_millis();
        let event = match &result {
            Ok(out) => TraceEvent::ok(
                &action,
                args,
                duration,
                serde_json::json!({
                    "model": out.model, "provider": out.provider, "stop_reason": out.stop_reason,
                    "usage": out.usage, "chars": out.text.chars().count(),
                }),
            ),
            Err(error) => TraceEvent::err(&action, args, duration, error.to_string()),
        };
        self.trace
            .record(event.with_execution(execution).with_kind("runner"))
            .await;
        result
    }

    /// What a person wrote, for the classifier: their last messages in the
    /// session and this run's prompt. Only a run a person started through
    /// an interface has any; services, webhooks and nested runs do not.
    fn user_said(
        &self,
        caller: &Caller,
        options: &RunOptions,
        session: Option<&agentd_sessions::SessionMeta>,
    ) -> Vec<String> {
        const KEPT: usize = 5;
        const LONGEST: usize = 2000;
        if caller.run_id.is_some() || caller.interface.is_none() {
            return Vec::new();
        }
        let mut said: Vec<String> = match (session, &self.sessions) {
            (Some(meta), Some(store)) => store
                .turns(&meta.id)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.role == agentd_ai::Role::User)
                .map(|m| m.content)
                .collect(),
            _ => Vec::new(),
        };
        said.extend(options.prompt.clone());
        let start = said.len().saturating_sub(KEPT);
        said.drain(..start);
        said.into_iter()
            .map(|text| text.chars().take(LONGEST).collect())
            .collect()
    }

    /// Fix who answers this run's questions and give the run its ids. Runs
    /// once, when the run starts; the values ride on the caller, which every
    /// check inside the run sees.
    fn start_run(
        &self,
        mut caller: Caller,
        runner_name: &str,
        options: &RunOptions,
        session: Option<&agentd_sessions::SessionMeta>,
    ) -> Result<(Caller, runs::RunGuard), RunnerError> {
        let runner_default = self
            .engine
            .load()
            .grants()
            .runner(runner_name)
            .and_then(|g| g.approval);
        let approval = agentd_runners::resolve_approval(
            runner_default,
            session.and_then(|m| m.approval),
            options.approval,
            options.admin,
            caller.approval,
        )?;
        // `classifier` with no classifier configured (a reload removed it)
        // runs as `human`, which is only ever stricter.
        let approval = if approval == ApprovalSetting::Classifier
            && self.classifier_name().is_none()
        {
            tracing::warn!(
                runner = runner_name,
                "approval resolved to `classifier`, but `[approval].classifier` names no runner, so this run uses `human`"
            );
            ApprovalSetting::Human
        } else {
            approval
        };
        let run_id = format!(
            "run-{}",
            RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let user_said = self.user_said(&caller, options, session);
        let guard = self
            .runs
            .start(&run_id, caller.run_id.as_deref(), user_said);
        caller.approval = Some(approval);
        caller.run_id = Some(run_id);
        caller.session_id = session.map(|m| m.id.clone());
        // A session's folder, else the folder of the run that started this
        // one (a nested run without a session keeps its parent's folder).
        caller.cwd = match session {
            Some(meta) => meta.cwd.clone(),
            None => caller.cwd,
        };
        Ok((caller, guard))
    }

    async fn run_runner_inner(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        mut options: RunOptions,
        sink: Option<agentd_ai::StreamSink>,
    ) -> Result<RunnerOutcome, RunnerError> {
        let Some(session_id) = options.session_id.take() else {
            let (caller, _run) = self.start_run(caller, runner_name, &options, None)?;
            let (out, _) = self
                .run_model(caller, runner_name, options, sink, None)
                .await?;
            return Ok(out);
        };
        let store = self.sessions.clone().ok_or_else(|| {
            RunnerError::Session(
                "no session store is configured, so `session_id` cannot be used".into(),
            )
        })?;
        // A session outside the caller's scope reads as missing: no oracle for
        // other tenants' ids.
        let scope = if options.admin {
            Scope::admin(caller.user.as_ref().map(|u| u.as_str()))
        } else {
            scope_of(&caller)
        };
        let meta = store
            .get_in(&scope, &session_id)
            .map_err(|e| RunnerError::Session(e.to_string()))?
            .ok_or_else(|| RunnerError::SessionNotFound(session_id.clone()))?;
        let (caller, _run) = self.start_run(caller, runner_name, &options, Some(&meta))?;
        let _guard = {
            let mut set = self.active_sessions.lock().unwrap();
            if !set.insert(session_id.clone()) {
                return Err(RunnerError::SessionBusy(session_id));
            }
            SessionGuard {
                id: session_id.clone(),
                set: self.active_sessions.clone(),
            }
        };
        let mut history = store
            .turns(&session_id)
            .map_err(|e| RunnerError::Session(e.to_string()))?;
        let def = self
            .runners
            .get(runner_name)
            .ok_or_else(|| RunnerError::NotFound(runner_name.to_string()))?;
        let policy = def.compact.clone().unwrap_or_default();
        if policy.enabled && meta.turn_count >= policy.after_turns {
            match self
                .compact_history(&caller, runner_name, &options, &policy, &history)
                .await
            {
                Ok(Some(compaction)) => {
                    let kept = history.split_off(compaction.drop_count);
                    history = std::iter::once(compaction.summary.clone())
                        .chain(kept)
                        .collect();
                    store
                        .compact(&session_id, compaction)
                        .map_err(|e| RunnerError::Session(e.to_string()))?;
                    tracing::info!(session = %session_id, runner = runner_name, "compacted session");
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(session = %session_id, runner = runner_name, error = %e, "session compaction failed; running with full history");
                }
            }
        }
        options.messages = Some(history);
        let mut partial = KeepPartial {
            store: store.clone(),
            session_id: session_id.clone(),
            progress: Progress::default(),
            finished: false,
        };
        let (mut out, new_turns) = self
            .run_model(caller, runner_name, options, sink, Some(&partial.progress))
            .await?;
        partial.finished = true;
        store
            .append(
                &session_id,
                &new_turns,
                out.usage.as_ref().unwrap_or(&Default::default()),
            )
            .map_err(|e| RunnerError::Session(e.to_string()))?;
        out.session_id = Some(session_id);
        Ok(out)
    }

    /// A provider with tools of its own reaches files and hosts outside the
    /// checks that enforce the operator's denials, so it may not run while
    /// any file or host is denied.
    fn refuse_own_tools_under_denials(
        &self,
        provider_name: &str,
        provider: &dyn agentd_ai::Provider,
    ) -> Result<(), RunnerError> {
        if !provider.has_own_tools() {
            return Ok(());
        }
        let engine = self.engine.load();
        let denied = engine
            .grants()
            .denial()
            .permissions
            .iter()
            .find(|p| matches!(p.parts().0, "fs.read" | "fs.write" | "net"))
            .cloned();
        match denied {
            Some(p) => Err(RunnerError::Denied(format!(
                "the `{provider_name}` provider runs its own tools, which reach files and hosts outside agent.d's checks, so it cannot run while grants.toml denies `{}`. Use a provider whose tool loop agent.d drives, such as `anthropic` or `openai`",
                p.as_str()
            ))),
            None => Ok(()),
        }
    }

    /// Summarize the oldest turns of `history` per `policy`. Returns `None`
    /// when there is nothing safe to drop (too few turns, or no clean
    /// user-turn boundary outside the tool-call groups).
    async fn compact_history(
        self: &Arc<Self>,
        caller: &Caller,
        runner_name: &str,
        options: &RunOptions,
        policy: &CompactPolicy,
        history: &[agentd_ai::Message],
    ) -> Result<Option<Compaction>, RunnerError> {
        let Some(drop_count) = compaction_cut(history, policy.keep_recent as usize) else {
            return Ok(None);
        };
        let def = self
            .runners
            .get(runner_name)
            .ok_or_else(|| RunnerError::NotFound(runner_name.to_string()))?;
        let composition = agentd_runners::compose(&def, &self.skills)?;
        let (provider_name, provider, model_id) =
            self.resolve_provider(&def, options.model.clone().or(composition.model.clone()))?;
        if let Decision::Deny { reason, .. } = self
            .engine
            .load()
            .check_runner_provider(runner_name, &provider_name)
        {
            return Err(RunnerError::Denied(reason));
        }
        self.refuse_own_tools_under_denials(&provider_name, provider.as_ref())?;
        let mut req = agentd_ai::CompletionRequest::default();
        if !composition.system.is_empty() {
            req.system = Some(composition.system.clone());
        }
        if !model_id.is_empty() {
            req.model = Some(model_id);
        }
        // Old turns may end mid tool-call group; the boundary rule above
        // guarantees they don't, but strip tool plumbing anyway so every
        // provider accepts the transcript as plain text.
        req.messages = history[..drop_count]
            .iter()
            .map(|m| agentd_ai::Message {
                role: match m.role {
                    agentd_ai::Role::Tool => agentd_ai::Role::User,
                    other => other,
                },
                content: if m.tool_calls.is_empty() {
                    m.content.clone()
                } else {
                    let calls: Vec<String> = m
                        .tool_calls
                        .iter()
                        .map(|c| format!("{}({})", c.name, c.arguments))
                        .collect();
                    format!("{}\n[called tools: {}]", m.content, calls.join(", "))
                },
                tool_calls: Vec::new(),
                tool_call_id: None,
                changes: Vec::new(),
                more: 0,
            })
            .collect();
        req.messages
            .push(agentd_ai::Message::user(policy.prompt.clone()));
        req.max_tokens = Some(1024);
        req.caller = Some(caller.clone());
        let resp = provider
            .complete(req)
            .await
            .map_err(|e| RunnerError::Provider {
                provider: provider_name,
                source: e,
            })?;
        let text = resp.text.trim().to_string();
        if text.is_empty() {
            return Err(RunnerError::Session(
                "the model returned an empty summary".into(),
            ));
        }
        Ok(Some(Compaction {
            drop_count,
            summary: agentd_ai::Message::user(format!("[Conversation summary]\n{text}")),
        }))
    }

    /// Resolve `(provider name, provider, upstream model id)` for a runner.
    fn resolve_provider(
        &self,
        def: &agentd_runners::RunnerDef,
        model: Option<String>,
    ) -> Result<(String, Arc<dyn agentd_ai::Provider>, String), RunnerError> {
        match model.as_deref() {
            Some(m) => self
                .providers
                .resolve_for_model(m)
                .ok_or_else(|| RunnerError::NoProvider {
                    name: def.name.clone(),
                    model: model.clone(),
                }),
            None => {
                let (n, p) =
                    self.providers
                        .resolve(None)
                        .ok_or_else(|| RunnerError::NoProvider {
                            name: def.name.clone(),
                            model: None,
                        })?;
                Ok((n, p, String::new()))
            }
        }
    }

    /// One model run over an explicit history. Returns the outcome plus every
    /// turn the run produced (user prompt, assistant replies, tool results) so
    /// a session can persist them.
    async fn run_model(
        self: &Arc<Self>,
        caller: Caller,
        runner_name: &str,
        options: RunOptions,
        sink: Option<agentd_ai::StreamSink>,
        progress: Option<&Progress>,
    ) -> Result<(RunnerOutcome, Vec<agentd_ai::Message>), RunnerError> {
        let RunOptions {
            prompt,
            messages,
            system: system_override,
            model: model_override,
            max_tokens,
            ..
        } = options;
        // `max_turns` stops runaway loops in one-off runs. A session run is a
        // chat someone can cancel, so it continues until the model is done.
        let turn_limit = caller.session_id.is_none().then_some(self.max_runner_turns);
        let def = self
            .runners
            .get(runner_name)
            .ok_or_else(|| RunnerError::NotFound(runner_name.to_string()))?;
        let composition = agentd_runners::compose(&def, &self.skills)?;

        let model_for_resolve = model_override.clone().or_else(|| composition.model.clone());
        let (provider_name, provider, model_id) = self.resolve_provider(&def, model_for_resolve)?;

        let tool_caller = {
            let mut c = caller.clone();
            c.runner = Some(runner_name.into());
            c
        };
        if let Decision::Deny { reason, .. } = self
            .engine
            .load()
            .check_runner_provider(runner_name, &provider_name)
        {
            return Err(RunnerError::Denied(reason));
        }
        self.refuse_own_tools_under_denials(&provider_name, provider.as_ref())?;
        let tools = self.build_tool_catalog(&composition.allowed_actions);

        let mut composed_system = composition.system.clone();
        if let Some(extra) = system_override {
            let extra = extra.trim();
            if !extra.is_empty() {
                if composed_system.is_empty() {
                    composed_system = extra.to_string();
                } else {
                    composed_system.push_str("\n\n");
                    composed_system.push_str(extra);
                }
            }
        }

        let mut req = agentd_ai::CompletionRequest::default();
        if !composed_system.is_empty() {
            req.system = Some(composed_system);
        }
        if !model_id.is_empty() {
            req.model = Some(model_id);
        }
        if let Some(msgs) = messages {
            req.messages = msgs;
        }
        // Everything pushed from here on is new this run.
        let base_len = req.messages.len();
        if let Some(p) = prompt
            && !p.is_empty()
        {
            req.messages.push(agentd_ai::Message::user(p));
        }
        if let Some(progress) = progress {
            *progress.lock().unwrap() = req.messages[base_len..].to_vec();
        }
        if req.messages.is_empty() {
            return Err(RunnerError::Provider {
                provider: provider_name,
                source: agentd_ai::ProviderError::Config(
                    "`runners.run` needs something to send the model — pass a `prompt` string or a `messages` list".into(),
                ),
            });
        }
        req.tools = tools;
        req.max_tokens = max_tokens;

        let req_model_echo = req.model.clone();
        match provider.loop_mode() {
            agentd_ai::LoopMode::ProviderOwned => {
                // Only stand up the MCP loopback when the runner actually has
                // tools to offer. A tool-less run stays plain text, which is
                // also the only shape text-only CLIs (codex exec) accept.
                let loopback = if req.tools.is_empty() {
                    None
                } else {
                    let dispatcher: Arc<dyn Dispatcher> = self.clone();
                    // A provider that bakes the token into a long-lived subprocess
                    // (codex) dictates a stable token; header-based providers
                    // (claude) get a fresh random one per invocation.
                    let token = provider
                        .preferred_mcp_token()
                        .unwrap_or_else(agentd_mcp::gen_token);
                    let loopback = agentd_mcp::bind_loopback(
                        dispatcher,
                        tool_caller.clone(),
                        req.tools.clone(),
                        token,
                    )
                    .await
                    .map_err(|e| RunnerError::Provider {
                        provider: provider_name.clone(),
                        source: agentd_ai::ProviderError::Config(format!(
                            "could not start the local MCP bridge that lets the model call tools ({e})"
                        )),
                    })?;
                    req.mcp_endpoint = Some(agentd_ai::McpEndpoint::Http {
                        url: loopback.url.clone(),
                        token: loopback.token.clone(),
                    });
                    req.dispatcher = Some(self.clone());
                    req.caller = Some(tool_caller.clone());
                    Some(loopback)
                };
                // The provider runs its own tool loop, so only the prompt and
                // the final reply are visible to us.
                let mut new_turns: Vec<agentd_ai::Message> = req.messages[base_len..].to_vec();
                let resp = match &sink {
                    Some(s) => provider.complete_streaming(req, s.clone()).await,
                    None => provider.complete(req).await,
                }
                .map_err(|e| RunnerError::Provider {
                    provider: provider_name.clone(),
                    source: e,
                })?;
                drop(loopback);
                new_turns.push(agentd_ai::Message::assistant(resp.text.clone()));
                Ok((
                    RunnerOutcome {
                        usage: resp.usage,
                        session_id: None,
                        text: resp.text,
                        provider: provider_name,
                        model: resp.model.or(req_model_echo),
                        stop_reason: resp.stop_reason,
                    },
                    new_turns,
                ))
            }
            agentd_ai::LoopMode::ExecutorOwned => {
                let mut turns = 0u32;
                let mut usage = Some(agentd_ai::types::Usage::default());
                loop {
                    if let Some(limit) = turn_limit.filter(|limit| turns >= *limit) {
                        let mut turns = req.messages.split_off(base_len);
                        // Close the exchange with an assistant turn so the
                        // stored history stays valid for the next message.
                        turns.push(agentd_ai::Message::assistant(format!(
                            "I stopped after using all {limit} turns without finishing."
                        )));
                        return Err(RunnerError::TurnLimit { limit, turns });
                    }
                    turns += 1;
                    let resp = match &sink {
                        Some(s) => provider.complete_streaming(req.clone(), s.clone()).await,
                        None => provider.complete(req.clone()).await,
                    }
                    .map_err(|e| RunnerError::Provider {
                        provider: provider_name.clone(),
                        source: e,
                    })?;
                    if sink.as_ref().is_some_and(|s| s.overflowed()) {
                        return Err(RunnerError::SlowConsumer);
                    }
                    match (&mut usage, &resp.usage) {
                        (Some(total), Some(turn)) => total.add(turn),
                        _ => usage = None,
                    }
                    if resp.tool_calls.is_empty() {
                        let mut new_turns = req.messages.split_off(base_len);
                        new_turns.push(agentd_ai::Message::assistant(resp.text.clone()));
                        return Ok((
                            RunnerOutcome {
                                usage,
                                session_id: None,
                                text: resp.text,
                                provider: provider_name,
                                model: resp.model.or(req_model_echo),
                                stop_reason: resp.stop_reason,
                            },
                            new_turns,
                        ));
                    }
                    req.messages.push(agentd_ai::Message {
                        role: agentd_ai::Role::Assistant,
                        content: resp.text.clone(),
                        tool_calls: resp.tool_calls.clone(),
                        tool_call_id: None,
                        changes: Vec::new(),
                        more: 0,
                    });
                    for call in resp.tool_calls {
                        let id = call.id.clone();
                        // The model may only call the tools this run offered,
                        // never any registered action it names.
                        if !req.tools.iter().any(|t| t.name == call.name) {
                            req.messages.push(agentd_ai::Message::tool_result(
                                id,
                                format!(
                                    "tool call failed (`{}` is not one of the tools this runner was given)",
                                    call.name
                                ),
                            ));
                            continue;
                        }
                        let action = ActionCall {
                            action: call.name.clone(),
                            args: call.arguments.clone(),
                        };
                        let result = self.run_with_changes(tool_caller.clone(), action).await;
                        let run_id = tool_caller.run_id.as_deref();
                        // Kept on the saved tool message too, so a reopened
                        // session shows the same files and diffs.
                        let (changes, more) = match &result {
                            Ok((_, _, changes)) => {
                                capture::cap_changes(self.redact_changes(run_id, changes))
                            }
                            Err(_) => (Vec::new(), 0),
                        };
                        if let Some(sink) = &sink {
                            let (output, error) = match &result {
                                Ok((res, _, _)) => {
                                    (self.runs.redact_secrets(run_id, &res.value), None)
                                }
                                Err((e, _)) => (serde_json::Value::Null, Some(e.to_string())),
                            };
                            let _ = sink.send(agentd_ai::StreamEvent::ToolResult {
                                name: call.name.clone(),
                                input: self.runs.redact_secrets(run_id, &call.arguments),
                                output,
                                error,
                                changes: changes.clone(),
                                more,
                            });
                        }
                        let text = match result {
                            Ok((res, _, _)) => {
                                serde_json::to_string(&res.value).unwrap_or_else(|e| {
                                    format!("the tool result could not be serialized to JSON ({e})")
                                })
                            }
                            Err((e, _)) => format!("tool call failed ({e})"),
                        };
                        req.messages.push(agentd_ai::Message {
                            changes,
                            more,
                            ..agentd_ai::Message::tool_result(id, text)
                        });
                    }
                    // Every call of this step has its result, so the history
                    // up to here stays valid if the run stops now.
                    if let Some(progress) = progress {
                        *progress.lock().unwrap() = req.messages[base_len..].to_vec();
                    }
                }
            }
        }
    }
}

/// The messages of a run so far, kept at each point where the history is
/// valid: after the prompt, and after every step whose calls all returned.
type Progress = std::sync::Mutex<Vec<agentd_ai::Message>>;

/// Saves what a session run did when it stops before finishing: cancelled,
/// failed, or dropped when the daemon shuts down. The prompt and every
/// finished step are kept, closed by a note, so the next message starts
/// from what really happened.
struct KeepPartial {
    store: Arc<dyn SessionStore>,
    session_id: String,
    progress: Progress,
    finished: bool,
}

impl Drop for KeepPartial {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut turns = std::mem::take(&mut *self.progress.lock().unwrap());
        if turns.is_empty() {
            return;
        }
        turns.push(agentd_ai::Message::assistant(
            "(This reply was stopped before it finished.)",
        ));
        if let Err(e) = self
            .store
            .append(&self.session_id, &turns, &Default::default())
        {
            tracing::warn!(session = %self.session_id, error = %e, "could not keep a stopped run's progress");
        }
    }
}

/// Session visibility for a caller: its interface (or service) plus user.
pub fn scope_of(caller: &Caller) -> Scope {
    Scope::from_caller(
        caller.interface.as_ref().map(|i| i.as_str()),
        caller.service.as_ref().map(|s| s.as_str()),
        caller.user.as_ref().map(|u| u.as_str()),
    )
}

/// Number of oldest turns to fold into a summary so that `keep_recent` turns
/// survive and the first surviving turn is a real user message (never a tool
/// result or an assistant turn whose tool calls would be orphaned). `None`
/// when no such cut exists.
fn compaction_cut(history: &[agentd_ai::Message], keep_recent: usize) -> Option<usize> {
    let target = history.len().checked_sub(keep_recent)?;
    if target == 0 {
        return None;
    }
    (1..=target).rev().find(|&cut| {
        let m = &history[cut];
        m.role == agentd_ai::Role::User && m.tool_call_id.is_none()
    })
}

fn parse_messages(v: serde_json::Value) -> Result<Vec<agentd_ai::Message>, RunnerError> {
    serde_json::from_value(v)
        .map_err(|e| RunnerError::InvalidInput(format!("invalid message history: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_runs_outside_a_session_get_a_default_deadline() {
        let one_off = RunOptions::default();
        assert_eq!(
            run_deadline(&one_off),
            Some(std::time::Duration::from_millis(
                RunOptions::DEFAULT_TIMEOUT_MS
            ))
        );
        let chat = RunOptions {
            session_id: Some("s".into()),
            ..Default::default()
        };
        assert_eq!(run_deadline(&chat), None);
        let bounded_chat = RunOptions {
            timeout_ms: Some(5_000),
            ..chat
        };
        assert_eq!(
            run_deadline(&bounded_chat),
            Some(std::time::Duration::from_millis(5_000))
        );
    }
}

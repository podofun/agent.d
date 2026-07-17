//! CodexAppServerProvider — drives `codex app-server` over JSON-RPC
//! (stdio). This is the codex provider for real tool use; the legacy
//! `CodexCliProvider` is text-only.
//!
//! # Design
//!
//! * One long-lived `codex app-server` subprocess per provider instance,
//!   lazily spawned on first call. Subsequent calls reuse it.
//! * Turns are serialized through an inbox mutex: one runner turn at a
//!   time. The codex side accepts concurrent threads, but the inbox is a
//!   single stream and demuxing by `threadId` only buys us parallelism
//!   we don't need today. Easy to lift later — change `inbox: Mutex<...>`
//!   to a per-thread fan-out and the call sites stay the same.
//! * For each call: open an ephemeral `thread/start`, send `turn/start`,
//!   drain notifications until `turn/completed`, collect agent text.
//!
//! # Approval bridge
//!
//! When the model triggers codex's built-in shell, file-write, or
//! permission-escalation flows, the app-server sends *server requests*:
//!
//! | Method                                  | What codex wants               |
//! |-----------------------------------------|--------------------------------|
//! | `item/commandExecution/requestApproval` | Permission to run a shell cmd  |
//! | `item/fileChange/requestApproval`       | Permission to write a file     |
//! | `item/permissions/requestApproval`      | Permission to widen sandbox    |
//! | `execCommandApproval` (legacy)          | Same as commandExecution above |
//! | `applyPatchApproval`   (legacy)         | Same as fileChange     above   |
//!
//! Shell and patch approvals are routed through the permission engine via
//! [`Dispatcher::check_grants`] (`codex.shell` / `codex.fs` tool grants), so
//! codex's built-ins clear the same 5-layer check as any other action — and
//! stay off unless the operator grants them. The two requests we can't decide
//! safely — `item/fileChange/requestApproval` (no per-file detail) and
//! `item/permissions/requestApproval` (codex widening its own sandbox) — are
//! always denied. See `handle_server_request` for the per-method mapping.

use crate::types::{
    CompletionRequest, CompletionResponse, LoopMode, McpEndpoint, Provider, ProviderError,
};
use agentd_codex::{
    Client, ClientInfo, Error as CodexError, Inbound, InitializeParams, ReviewDecision,
    ReviewDecisionResp, ThreadStartParams, ThreadStartResult, TurnStartParams, UserInput,
};
use agentd_permissions::{Caller, PermissionSet};
use agentd_types::{Dispatcher, GrantDecision};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// Env var the codex subprocess reads its MCP bearer token from. codex's
/// `mcp_servers.<name>.bearer_token_env_var` names this var; we set it on the
/// child at spawn. (codex auths HTTP MCP via an env-var indirection, not a
/// literal token in the config.)
const MCP_BEARER_ENV: &str = "AGENTD_MCP_BEARER";

pub struct CodexAppServerProvider {
    bin: String,
    name: String,
    state: Mutex<Option<ActiveClient>>,
    /// Stable bearer for the agentd MCP loopback. The subprocess is spawned
    /// once and reused, so the token can't change per call — it's fixed for the
    /// provider's life, set as `MCP_BEARER_ENV` at spawn, and the executor is
    /// told to bind every loopback for this provider with it.
    mcp_token: String,
}

struct ActiveClient {
    client: Client,
    /// Mutex around the inbox receiver so turns serialize cleanly.
    inbox: Arc<Mutex<mpsc::UnboundedReceiver<Inbound>>>,
}

impl CodexAppServerProvider {
    pub fn new() -> Self {
        Self {
            bin: "codex".into(),
            name: "openai".into(),
            state: Mutex::new(None),
            mcp_token: agentd_mcp_token(),
        }
    }
    pub fn with_bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

impl Default for CodexAppServerProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for CodexAppServerProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn loop_mode(&self) -> LoopMode {
        LoopMode::ProviderOwned
    }

    fn preferred_mcp_token(&self) -> Option<String> {
        Some(self.mcp_token.clone())
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let active = self.ensure_active().await?;
        // Serialize turns: lock the inbox for the lifetime of this call.
        let mut inbox = active.inbox.lock().await;

        let model = req.model.clone();

        // Build the codex `config` payload we send via thread/start.
        // Two things go in here:
        //
        // 1. The MCP loopback registration so dynamic tool dispatch
        //    rides agentd's permission engine. `approval_mode =
        //    "always_allow"` is safe: real gating happens inside the
        //    loopback handler.
        // 2. Disable codex's built-in tools so the model can only act
        //    through agentd's MCP loopback — same posture as claude w/
        //    `--allowedTools mcp__agentd__*`. Codex's approval bridge
        //    proved unreliable for /tmp + cwd writes (the "agent" exec
        //    source bypasses approvals for sandbox-permitted writes),
        //    so the safer model is to turn the built-ins off entirely.
        //
        //    Names compiled into codex 0.130 under `[features]`:
        //      shell_tool, unified_exec, web_search, view_image,
        //      include_apply_patch_tool, memory_tool, js_repl_tool,
        //      js_repl, code_mode, code_mode_only, shell_zsh_fork,
        //      shell_snapshot, telepathy, codex_hooks.
        //    Setting them all to `false` leaves the agentd MCP server
        //    as the only addressable tool surface.
        let mut config_map = serde_json::Map::new();
        config_map.insert(
            "features".into(),
            serde_json::json!({
                "shell_tool": false,
                "unified_exec": false,
                "web_search": false,
                "view_image": false,
                "include_apply_patch_tool": false,
                "memory_tool": false,
                "js_repl_tool": false,
                "js_repl": false,
                "code_mode": false,
                "code_mode_only": false,
                "shell_zsh_fork": false,
                "shell_snapshot": false,
                "telepathy": false,
                "codex_hooks": false,
            }),
        );
        if let Some(McpEndpoint::Http { url, .. }) = &req.mcp_endpoint
            && !req.tools.is_empty()
        {
            // codex reads the bearer from the env var named here (set on the
            // subprocess at spawn) and sends it as `Authorization: Bearer ...`,
            // which is what the loopback checks. The executor binds the loopback
            // with `self.mcp_token`, the same value the env var holds.
            config_map.insert(
                "mcp_servers".into(),
                serde_json::json!({
                    "agentd": {
                        "url": url,
                        "approval_mode": "always_allow",
                        "bearer_token_env_var": MCP_BEARER_ENV
                    }
                }),
            );
        }
        let mcp_config = Some(Value::Object(config_map));

        // Open an ephemeral thread for this turn. Cheap (no rollout file
        // persisted) and keeps state isolated across runner calls.
        let thread_params = ThreadStartParams {
            model: model.clone(),
            sandbox: Some("read-only".into()),
            // `untrusted` makes codex escalate approval for *every*
            // shell command outside the trusted set (which we leave
            // empty). `on-request` left codex deciding when to ask,
            // and in practice codex never asked for /tmp writes
            // because Landlock leaves /tmp writable in read-only mode.
            // `untrusted` forces the round-trip we need to gate.
            approval_policy: Some("untrusted".into()),
            ephemeral: Some(true),
            developer_instructions: req.system.clone(),
            config: mcp_config,
            ..Default::default()
        };
        let thread_resp = active
            .client
            .request(
                "thread/start",
                serde_json::to_value(thread_params)
                    .map_err(|e| ProviderError::Config(format!("thread/start params: {e}")))?,
            )
            .await
            .map_err(map_err)?;
        let thread: ThreadStartResult = serde_json::from_value(thread_resp)
            .map_err(|e| ProviderError::Upstream(format!("thread/start shape: {e}")))?;
        let thread_id = thread.thread.id;

        // Build the turn input. We collapse system+messages+prompt into
        // one text block — codex's chat history lives on the thread, but
        // since each call uses a fresh thread we can encode everything
        // here.
        let body = req.flatten();
        let turn_params = TurnStartParams {
            thread_id: thread_id.clone(),
            input: vec![UserInput::text(body)],
            model,
            // Lower reasoning effort so short agent turns don't burn
            // ~30+ s reasoning per call.
            effort: Some("low".into()),
        };
        active
            .client
            .request(
                "turn/start",
                serde_json::to_value(turn_params)
                    .map_err(|e| ProviderError::Config(format!("turn/start params: {e}")))?,
            )
            .await
            .map_err(map_err)?;

        // Drain events until turn/completed (for our thread).
        let mut final_text = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
        loop {
            let recv = tokio::time::timeout_at(deadline, inbox.recv())
                .await
                .map_err(|_| ProviderError::Transport("turn timeout".into()))?;
            let Some(ev) = recv else {
                return Err(ProviderError::Transport("app-server inbox closed".into()));
            };
            match ev {
                Inbound::Notification { method, params } => {
                    tracing::trace!(method = %method, "codex notification");
                    if !event_belongs_to_thread(&params, &thread_id) {
                        continue;
                    }
                    match method.as_str() {
                        "item/completed" => {
                            if let Some(item) = params.get("item")
                                && item.get("type").and_then(|t| t.as_str()) == Some("agentMessage")
                                && let Some(text) = item.get("text").and_then(|t| t.as_str())
                            {
                                if !final_text.is_empty() {
                                    final_text.push('\n');
                                }
                                final_text.push_str(text);
                            }
                        }
                        "turn/completed" => break,
                        "error" => {
                            return Err(ProviderError::Upstream(format!("codex error: {params}")));
                        }
                        _ => {}
                    }
                }
                Inbound::ServerRequest { id, method, params } => {
                    tracing::debug!(method = %method, "codex server request");
                    handle_server_request(
                        &active.client,
                        id,
                        &method,
                        &params,
                        req.dispatcher.as_ref(),
                        req.caller.as_ref(),
                    )
                    .await;
                }
            }
        }

        if final_text.is_empty() {
            return Err(ProviderError::EmptyResponse);
        }
        Ok(CompletionResponse {
            text: final_text,
            model: req.model,
            stop_reason: Some("end_turn".into()),
            tool_calls: Vec::new(),
        })
    }
}

impl CodexAppServerProvider {
    /// Lazy-spawn the app-server on first use.
    async fn ensure_active(&self) -> Result<ActiveClient, ProviderError> {
        let mut slot = self.state.lock().await;
        if let Some(a) = slot.as_ref() {
            return Ok(ActiveClient {
                client: a.client.clone(),
                inbox: a.inbox.clone(),
            });
        }
        // Hand the loopback bearer to the subprocess via the env var codex's
        // `bearer_token_env_var` points at. Set once, reused for the subprocess's
        // life — which is why the token must be stable (see `mcp_token`).
        let (client, inbox) = Client::spawn_with_env(
            &self.bin,
            &[(MCP_BEARER_ENV.into(), self.mcp_token.clone())],
        )
        .await
        .map_err(map_err)?;
        // Initialize once.
        let _init = client
            .request(
                "initialize",
                serde_json::to_value(InitializeParams {
                    client_info: ClientInfo {
                        name: "agentd".into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                    },
                })
                .map_err(|e| ProviderError::Config(format!("initialize params: {e}")))?,
            )
            .await
            .map_err(map_err)?;
        let inbox = Arc::new(Mutex::new(inbox));
        let active = ActiveClient {
            client: client.clone(),
            inbox: inbox.clone(),
        };
        *slot = Some(ActiveClient { client, inbox });
        Ok(active)
    }
}

/// Bridge codex approval requests back to the agentd permission engine.
///
/// Method-by-method:
///
/// * **MCP tool calls** (`mcpServer/elicitation/request` with
///   `_meta.codex_approval_kind == "mcp_tool_call"` and
///   `serverName == "agentd"`) — accept. The real gate lives inside the
///   agentd MCP loopback handler, which routes each call through
///   `Executor::run` and the 5-layer engine. Asking codex's approval
///   layer on top would be a double-prompt with no extra safety.
/// * **`execCommandApproval` / `item/commandExecution/requestApproval`** —
///   ask the agentd engine whether the runner is allowed to use the
///   `codex.shell` tool with `shell.exec:<bin>` granted. Grants look
///   like `[tool."codex.shell"] granted = ["shell.exec:*"]` plus the
///   runner's `allowed_actions` containing `codex.shell`.
/// * **`applyPatchApproval`** — `fileChanges` keys are absolute paths.
///   Each path must clear `fs.write:<path>` on the `codex.fs` tool.
///   All-or-nothing: if any one is denied, the whole patch is denied.
/// * **`item/fileChange/requestApproval`** — codex 0.130 sends this
///   without per-file info; we can't decide safely, deny.
/// * **`item/permissions/requestApproval`** — codex asking to widen its
///   own sandbox; never allowed.
///
/// `dispatcher` + `caller` come from `CompletionRequest` (the executor
/// fills them for ProviderOwned providers). If either is missing the
/// bridge falls back to deny — codex would have run unconstrained
/// otherwise.
async fn handle_server_request(
    client: &Client,
    id: Value,
    method: &str,
    params: &Value,
    dispatcher: Option<&Arc<dyn Dispatcher>>,
    caller: Option<&Caller>,
) {
    match method {
        "mcpServer/elicitation/request" => {
            let server_name = params.get("serverName").and_then(|v| v.as_str());
            let kind = params
                .get("_meta")
                .and_then(|m| m.get("codex_approval_kind"))
                .and_then(|v| v.as_str());
            if server_name == Some("agentd") && kind == Some("mcp_tool_call") {
                let body = serde_json::json!({ "action": "accept", "content": {} });
                let _ = client.reply(id, body).await;
            } else {
                let body = serde_json::json!({ "action": "decline" });
                let _ = client.reply(id, body).await;
            }
        }
        // Legacy approval methods use the `approved`/`denied` enum.
        "execCommandApproval" => {
            let decision = match (dispatcher, caller) {
                (Some(d), Some(c)) => check_shell_grants(d.as_ref(), c.clone(), params).await,
                _ => GrantDecision::Deny("no dispatcher/caller wired".into()),
            };
            reply_legacy(client, id, decision).await;
        }
        "applyPatchApproval" => {
            let decision = match (dispatcher, caller) {
                (Some(d), Some(c)) => check_patch_grants(d.as_ref(), c.clone(), params).await,
                _ => GrantDecision::Deny("no dispatcher/caller wired".into()),
            };
            reply_legacy(client, id, decision).await;
        }
        // Newer `item/*RequestApproval` methods use the
        // `accept`/`decline` enum AND can amend execpolicy / network
        // policy via object variants. For shell we reply with
        // `acceptWithExecpolicyAmendment` echoing codex's proposed
        // amendment so the actual OS-level sandbox loosens — bare
        // `accept` would clear the model's permission layer but the
        // read-only sandbox would still block the write.
        "item/commandExecution/requestApproval" => {
            let decision = match (dispatcher, caller) {
                (Some(d), Some(c)) => check_shell_grants(d.as_ref(), c.clone(), params).await,
                _ => GrantDecision::Deny("no dispatcher/caller wired".into()),
            };
            reply_command_exec(client, id, decision, params).await;
        }
        "item/fileChange/requestApproval" | "item/permissions/requestApproval" => {
            // No actionable info / never-allowed escalation.
            let _ = client
                .reply(id, serde_json::json!({ "decision": "decline" }))
                .await;
        }
        _ => {
            // Unhandled: reply empty so codex isn't left hanging.
            let _ = client.reply(id, Value::Object(Default::default())).await;
        }
    }
}

/// Reply for legacy `execCommandApproval` / `applyPatchApproval`.
async fn reply_legacy(client: &Client, id: Value, decision: GrantDecision) {
    let body = serde_json::to_value(ReviewDecisionResp {
        decision: if decision.is_allow() {
            ReviewDecision::Approved
        } else {
            ReviewDecision::Denied
        },
    })
    .expect("ReviewDecisionResp must serialize");
    tracing::debug!(?decision, "codex approval bridge legacy");
    let _ = client.reply(id, body).await;
}

/// Reply for `item/commandExecution/requestApproval`. On allow we echo codex's
/// `proposedExecpolicyAmendment` so the actual OS-level sandbox loosens
/// for THIS command - plain `accept` alone clears the approval layer but
/// the read-only sandbox would still block writes.
async fn reply_command_exec(client: &Client, id: Value, decision: GrantDecision, params: &Value) {
    tracing::debug!(?decision, "codex approval bridge command-exec");
    let body = if decision.is_allow() {
        match params.get("proposedExecpolicyAmendment") {
            Some(amendment) => serde_json::json!({
                "decision": { "acceptWithExecpolicyAmendment": {
                    "execpolicy_amendment": amendment,
                } }
            }),
            // No amendment proposed (read-only command): plain accept.
            None => serde_json::json!({ "decision": "accept" }),
        }
    } else {
        serde_json::json!({ "decision": "decline" })
    };
    let _ = client.reply(id, body).await;
}

/// `execCommandApproval.command` is `Vec<String>` (argv);
/// `item/commandExecution/requestApproval.command` is an optional single
/// string. Normalize both to a bin name + check `shell.exec:<bin>`
/// against the `codex.shell` tool.
async fn check_shell_grants(
    dispatcher: &dyn Dispatcher,
    caller: Caller,
    params: &Value,
) -> GrantDecision {
    let bin = if let Some(arr) = params.get("command").and_then(|v| v.as_array()) {
        arr.first().and_then(|v| v.as_str()).map(|s| s.to_string())
    } else if let Some(s) = params.get("command").and_then(|v| v.as_str()) {
        // Crude split — codex's free-form command string isn't argv. We
        // grab the first whitespace-bounded token; users who want
        // tighter gating should rely on the executable name being the
        // first token (it almost always is).
        s.split_whitespace().next().map(|s| s.to_string())
    } else {
        None
    };
    let Some(bin) = bin else {
        return GrantDecision::Deny("missing command".into());
    };
    let perm = format!("shell.exec:{}", bin);
    let required = PermissionSet::from_iter([perm]);
    dispatcher
        .check_grants(caller, "codex.shell", required)
        .await
}

/// `applyPatchApproval.fileChanges` is an object keyed by absolute path.
/// Build `fs.write:<path>` for each and require all to pass under the
/// `codex.fs` tool.
async fn check_patch_grants(
    dispatcher: &dyn Dispatcher,
    caller: Caller,
    params: &Value,
) -> GrantDecision {
    let Some(changes) = params.get("fileChanges").and_then(|v| v.as_object()) else {
        return GrantDecision::Deny("missing fileChanges".into());
    };
    if changes.is_empty() {
        return GrantDecision::Deny("empty fileChanges".into());
    }
    let perms: Vec<String> = changes.keys().map(|p| format!("fs.write:{}", p)).collect();
    let required = PermissionSet::from_iter(perms);
    dispatcher.check_grants(caller, "codex.fs", required).await
}

fn event_belongs_to_thread(params: &Value, thread_id: &str) -> bool {
    // turn/completed + item/* events carry threadId at the top level.
    if let Some(t) = params.get("threadId").and_then(|v| v.as_str()) {
        return t == thread_id;
    }
    // Some events nest the thread under `thread.id` (e.g. thread/started).
    // Those we ignore for routing — they aren't completion markers.
    if let Some(t) = params
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
    {
        return t == thread_id;
    }
    // Unknown shape
    true
}

fn map_err(e: CodexError) -> ProviderError {
    match e {
        CodexError::Spawn(io) => ProviderError::Transport(format!("spawn codex app-server: {io}")),
        CodexError::Io(io) => ProviderError::Transport(format!("io: {io}")),
        CodexError::Serde(s) => {
            ProviderError::Config(format!("invalid codex-app-server message: {s}"))
        }
        CodexError::Transport(s) => ProviderError::Transport(s),
        CodexError::Rpc { code, message } => {
            ProviderError::Upstream(format!("codex rpc {code}: {message}"))
        }
        CodexError::BadResponse(id) => {
            ProviderError::Transport(format!("missing response for id {id}"))
        }
    }
}

/// 256 bits of OS randomness, hex-encoded — the provider's stable MCP bearer.
fn agentd_mcp_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS RNG unavailable");
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in buf {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_types::{ActionCall, ActionResult, RegistryError};
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[test]
    fn default_name_is_openai() {
        assert_eq!(CodexAppServerProvider::new().name(), "openai");
    }

    #[test]
    fn loop_mode_is_provider_owned() {
        assert_eq!(
            CodexAppServerProvider::new().loop_mode(),
            LoopMode::ProviderOwned
        );
    }

    #[test]
    fn event_belongs_to_thread_top_level() {
        let v = serde_json::json!({ "threadId": "abc", "x": 1 });
        assert!(event_belongs_to_thread(&v, "abc"));
        assert!(!event_belongs_to_thread(&v, "other"));
    }

    #[test]
    fn event_belongs_to_thread_nested() {
        let v = serde_json::json!({ "thread": { "id": "xyz" } });
        assert!(event_belongs_to_thread(&v, "xyz"));
        assert!(!event_belongs_to_thread(&v, "nope"));
    }

    /// Stub dispatcher that records each `check_grants` call and replies
    /// per the configured plan. Lets us assert the codex provider asks
    /// the engine the right questions for each codex approval shape.
    struct GrantStub {
        plan: Mutex<Vec<GrantDecision>>,
        seen: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl GrantStub {
        fn allow_once() -> Arc<Self> {
            Arc::new(Self {
                plan: Mutex::new(vec![GrantDecision::Allow]),
                seen: Mutex::new(Vec::new()),
            })
        }
        fn deny_once(reason: &str) -> Arc<Self> {
            Arc::new(Self {
                plan: Mutex::new(vec![GrantDecision::Deny(reason.into())]),
                seen: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Dispatcher for GrantStub {
        async fn dispatch(
            &self,
            _: Caller,
            _: ActionCall,
        ) -> Result<(ActionResult, u128), (RegistryError, u128)> {
            unreachable!("dispatch must not be called from approval bridge")
        }
        async fn check_grants(
            &self,
            _caller: Caller,
            tool: &str,
            required: PermissionSet,
        ) -> GrantDecision {
            let mut perms: Vec<String> = required.iter().map(|p| p.as_str().to_string()).collect();
            perms.sort();
            self.seen.lock().unwrap().push((tool.to_string(), perms));
            self.plan
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(GrantDecision::Deny("plan exhausted".into()))
        }
    }

    #[tokio::test]
    async fn shell_bridge_asks_codex_shell_with_bin_slug_argv_form() {
        let stub = GrantStub::allow_once();
        let dispatcher: Arc<dyn Dispatcher> = stub.clone();
        let params = serde_json::json!({
            "command": ["ls", "-la", "/tmp"],
            "callId": "x",
            "conversationId": "t",
            "cwd": "/tmp",
            "parsedCmd": []
        });
        let decision = check_shell_grants(dispatcher.as_ref(), Caller::default(), &params).await;
        assert!(decision.is_allow());
        let seen = stub.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "codex.shell");
        assert_eq!(seen[0].1, vec!["shell.exec:ls".to_string()]);
    }

    #[tokio::test]
    async fn shell_bridge_handles_string_command() {
        let stub = GrantStub::deny_once("policy");
        let dispatcher: Arc<dyn Dispatcher> = stub.clone();
        let params = serde_json::json!({ "command": "rm -rf /" });
        let decision = check_shell_grants(dispatcher.as_ref(), Caller::default(), &params).await;
        assert!(!decision.is_allow());
        let seen = stub.seen.lock().unwrap().clone();
        assert_eq!(seen[0].1, vec!["shell.exec:rm".to_string()]);
    }

    #[tokio::test]
    async fn shell_bridge_denies_without_command() {
        let stub = GrantStub::allow_once();
        let dispatcher: Arc<dyn Dispatcher> = stub.clone();
        let params = serde_json::json!({});
        let decision = check_shell_grants(dispatcher.as_ref(), Caller::default(), &params).await;
        assert!(!decision.is_allow());
        assert!(stub.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn patch_bridge_collects_all_paths() {
        let stub = GrantStub::allow_once();
        let dispatcher: Arc<dyn Dispatcher> = stub.clone();
        let params = serde_json::json!({
            "callId": "x",
            "conversationId": "t",
            "fileChanges": {
                "/tmp/a.txt": { "type": "add", "content": "x" },
                "/tmp/b.txt": { "type": "add", "content": "y" }
            }
        });
        let decision = check_patch_grants(dispatcher.as_ref(), Caller::default(), &params).await;
        assert!(decision.is_allow());
        let seen = stub.seen.lock().unwrap().clone();
        assert_eq!(seen[0].0, "codex.fs");
        assert_eq!(
            seen[0].1,
            vec![
                "fs.write:/tmp/a.txt".to_string(),
                "fs.write:/tmp/b.txt".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn patch_bridge_denies_empty() {
        let stub = GrantStub::allow_once();
        let dispatcher: Arc<dyn Dispatcher> = stub.clone();
        let params = serde_json::json!({ "fileChanges": {} });
        let decision = check_patch_grants(dispatcher.as_ref(), Caller::default(), &params).await;
        assert!(!decision.is_allow());
        assert!(stub.seen.lock().unwrap().is_empty());
    }
}

//! Full-stack live test: real `claude` CLI driving a real Lua action via
//! the MCP loopback, gated through the actual permission engine.
//!
//! What this proves end-to-end:
//!
//! 1. Executor binds an MCP loopback against itself (`Arc<Executor>` as
//!    `Dispatcher`).
//! 2. `ClaudeCliProvider` spawns `claude -p` w/ `--mcp-config` pointing at
//!    that loopback.
//! 3. claude calls `notes.lookup` over MCP HTTP.
//! 4. MCP server forwards to `Executor::run`.
//! 5. Permission engine: tool grants OK, runner allowlist OK, policy OK.
//! 6. Registry (LuaHost) invokes the Lua handler.
//! 7. Lua handler runs `agentd.context.fs.write(marker, ...)` — that
//!    creates a real file we can stat from the test, proving the handler
//!    actually executed (not just a hallucinated response).
//! 8. Handler returns `{ found = ... }`, marshalled back through MCP →
//!    claude → final assistant message.
//!
//! Gated `AGENTD_TEST_CLAUDE=1`. Costs tokens; needs logged-in `claude`.

use std::sync::Arc;

use agentd_ai::{ClaudeCliProvider, ProviderRegistry};
use agentd_executor::Executor;
use agentd_permissions::{
    Caller, Engine, Grants, GrantsFile, RunnerGrants, ToolGrants, model::PermissionSet,
};
use agentd_scripting::LuaHost;
use agentd_secrets::MemoryStore;
use agentd_trace::{TraceEvent, TraceSink};
use agentd_types::Registry;
use async_trait::async_trait;

fn gated() -> bool {
    std::env::var("AGENTD_TEST_CLAUDE").ok().as_deref() == Some("1")
}

struct NullSink;
#[async_trait]
impl TraceSink for NullSink {
    async fn record(&self, _e: TraceEvent) {}
}

#[tokio::test(flavor = "multi_thread")]
async fn live_full_loop_lua_action_through_cli_and_mcp() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_CLAUDE=1");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("called.marker");
    let marker_str = marker.to_string_lossy().to_string();

    // init.lua registers the tool + action + runner. The handler writes a
    // marker file (proving the Lua code actually ran) and returns a payload
    // whose `found` field claude is instructed to quote verbatim.
    let init = format!(
        r#"
        local marker = {marker_lit}

        agentd.tool{{ name = "notes" }}

        agentd.register{{
          name = "notes.lookup",
          handler = function(args)
            local q = (args and args.q) or "no-q"
            agentd.context.fs.write(marker, "called: " .. q)
            return {{ found = "the agentd answer is 42" }}
          end,
        }}

        agentd.runner{{
          name = "researcher",
          system =
            "When the user asks for the agentd answer, you MUST call the " ..
            "`notes.lookup` tool exactly once with arguments {{q=\"answer\"}}, " ..
            "then quote the value of the `found` field verbatim in your reply.",
          model = "anthropic-cli/claude-haiku-4-5-20251001",
          actions = {{ "notes.lookup" }},
        }}
        "#,
        marker_lit = lua_string_literal(&marker_str),
    );
    let init_path = dir.path().join("init.lua");
    std::fs::write(&init_path, init).unwrap();

    // Stand up the Lua host. MemoryStore for secrets (no keyring needed).
    let host = LuaHost::new().expect("lua host");
    host.set_root(dir.path());
    host.start_async_runtime(tokio::runtime::Handle::current());
    host.set_secrets(Arc::new(MemoryStore::default()));

    // Provider registry: `anthropic-cli` → ClaudeCliProvider. We pin the
    // model string in the runner def, so the registry just needs the CLI
    // provider registered under that name.
    let cli: Arc<dyn agentd_ai::Provider> = Arc::new(ClaudeCliProvider::new());
    let mut providers = ProviderRegistry::new();
    providers.insert("anthropic-cli", cli.clone());
    providers.set_default("anthropic-cli");
    let providers = Arc::new(providers);
    host.set_ai_provider("anthropic-cli", cli);
    host.set_default_ai_provider("anthropic-cli");

    host.load_file(&init_path).expect("init.lua eval");

    // Grants. The `notes` tool needs `fs.write:<marker>` so the action's
    // `agentd.context.fs.write(marker, ...)` clears the inline perm check.
    // The runner `researcher` has `notes.lookup` on its allowlist.
    let mut file = GrantsFile::default();
    file.tool.insert(
        "notes".into(),
        ToolGrants {
            granted: PermissionSet::from_iter([format!("fs.write:{}", marker_str)]),
        },
    );
    let mut runner_grants = RunnerGrants::default();
    runner_grants.allowed_actions.insert("notes.lookup".into());
    file.runner.insert("researcher".into(), runner_grants);
    let engine = Arc::new(Engine::new(Grants::from_file(file)));

    let skills = host.skills();
    let runners = host.runners();
    let services = host.services();

    let registry: Arc<dyn Registry> = Arc::new(host);
    let exec = Arc::new(Executor::new(
        registry,
        Arc::new(NullSink),
        engine,
        runners,
        services,
        skills,
        providers,
    ));

    let outcome = exec
        .run_runner(
            Caller::interface("ws").with_runner("researcher"),
            "researcher",
            "Look up the agentd answer using your tool and tell me what it says.".into(),
        )
        .await
        .expect("runner run");

    // 1. Lua action actually ran — marker file exists w/ the call's payload.
    let contents =
        std::fs::read_to_string(&marker).expect("marker file missing — Lua handler never ran");
    assert!(
        contents.starts_with("called:"),
        "marker contents unexpected: {contents:?}"
    );

    // 2. Final assistant text quotes the tool result. We don't pin exact
    //    wording but the magic number from `found` must be present.
    assert!(
        outcome.text.contains("42"),
        "expected `42` in reply, got: {}",
        outcome.text
    );

    // 3. ProviderOwned semantics — executor never saw tool_calls itself.
    assert_eq!(outcome.provider, "anthropic-cli");
}

/// Escape a path for embedding inside a Lua string literal.
fn lua_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

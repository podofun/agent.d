//! Inline (mid-handler) approval escalation: a `ctx.*` capability call that
//! fails its permission check yields to the wired `InlineApprovals` hook, and
//! an approving verdict retries the call under the new grant.

use std::io::Write;
use std::sync::{Arc, Mutex};

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{
    ActionCall, CallContext, InlineApprovalRequest, InlineApprovals, Registry, Verdict,
};
use async_trait::async_trait;

fn write_tools(scripts: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in scripts {
        let p = dir.path().join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }
    dir
}

fn ctx(grants: &[&str]) -> CallContext {
    CallContext {
        caller: Caller::interface("test"),
        effective_grants: PermissionSet::from_iter(grants.iter().copied()),
        call_chain: Vec::new(),
        cwd: None,
    }
}

fn lua_lit(p: impl AsRef<str>) -> String {
    p.as_ref().replace('\\', "\\\\")
}

/// Hook returning a scripted verdict; records every request it sees.
struct StubHook {
    verdict: Verdict,
    seen: Mutex<Vec<InlineApprovalRequest>>,
}
impl StubHook {
    fn new(verdict: Verdict) -> Arc<Self> {
        Arc::new(Self {
            verdict,
            seen: Mutex::new(Vec::new()),
        })
    }
}
#[async_trait]
impl InlineApprovals for StubHook {
    async fn request_inline(&self, req: InlineApprovalRequest) -> Verdict {
        self.seen.lock().unwrap().push(req);
        self.verdict
    }
}

fn read_action_script(target: &str) -> String {
    format!(
        r#"
        agentd.action{{
          name = "r.read",
          handler = function(_, ctx)
            return {{ s = ctx.fs.read("{target}") }}
          end,
        }}
        "#
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn allow_once_retries_and_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("hello.txt");
    std::fs::write(&target, "hello approver").unwrap();
    let target_str = lua_lit(target.to_string_lossy());

    let dir = write_tools(&[("t.lua", &read_action_script(&target_str))]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let hook = StubHook::new(Verdict::AllowOnce);
    host.set_inline_approvals(hook.clone());

    // Mirror the executor: the dispatch seeds the chain with the action name.
    let mut call_ctx = ctx(&[]);
    call_ctx.call_chain = vec!["r.read".into()];
    let res = host
        .call(
            call_ctx,
            ActionCall {
                action: "r.read".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .expect("approved call should succeed");
    assert_eq!(res.value["s"], "hello approver");

    let seen = hook.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "exactly one escalation");
    let req = &seen[0];
    assert!(
        req.permission.starts_with("fs.read:"),
        "got {}",
        req.permission
    );
    assert_eq!(req.call_chain, vec!["r.read".to_string()]);
    assert_eq!(req.grant_kind.as_deref(), Some("tool"));
    assert_eq!(req.grant_name.as_deref(), Some("r"));
}

#[tokio::test(flavor = "multi_thread")]
async fn deny_verdict_raises_clean_denial() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("hello.txt");
    std::fs::write(&target, "nope").unwrap();
    let target_str = lua_lit(target.to_string_lossy());

    let dir = write_tools(&[("t.lua", &read_action_script(&target_str))]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    host.set_inline_approvals(StubHook::new(Verdict::Deny));

    let err = host
        .call(
            ctx(&[]),
            ActionCall {
                action: "r.read".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("permission denied"), "got {msg}");
    assert!(msg.contains("fs.read"), "got {msg}");
    // The internal escalation mark must never leak to the user.
    assert!(!msg.contains('\u{1}'), "control mark leaked: {msg:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_hook_keeps_fail_fast_denial() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("hello.txt");
    std::fs::write(&target, "x").unwrap();
    let target_str = lua_lit(target.to_string_lossy());

    let dir = write_tools(&[("t.lua", &read_action_script(&target_str))]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();

    let err = host
        .call(
            ctx(&[]),
            ActionCall {
                action: "r.read".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("fs.read"), "got {msg}");
    assert!(!msg.contains('\u{1}'), "control mark leaked: {msg:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn granted_call_never_consults_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("hello.txt");
    std::fs::write(&target, "granted").unwrap();
    let target_str = lua_lit(target.to_string_lossy());
    let glob = format!("fs.read:{}/**", tmp.path().display());

    let dir = write_tools(&[("t.lua", &read_action_script(&target_str))]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let hook = StubHook::new(Verdict::Deny);
    host.set_inline_approvals(hook.clone());

    let res = host
        .call(
            ctx(&[&glob]),
            ActionCall {
                action: "r.read".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .expect("granted call must succeed");
    assert_eq!(res.value["s"], "granted");
    assert!(hook.seen.lock().unwrap().is_empty(), "hook must stay idle");
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_and_memory_paths_escalate_too() {
    // `ctx.memory` gates on `memory.read:<ns>`/`memory.write:<ns>` through
    // per-namespace closures — the wrapper must escalate there as well.
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{
          name = "m.touch",
          handler = function(_, ctx)
            local ns = ctx.memory.create("scratch")
            ns:set("k", "v")
            return { v = ns:get("k") }
          end,
        }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    host.set_memory(Arc::new(agentd_memory::MemMemoryStore::new()));
    let hook = StubHook::new(Verdict::AllowOnce);
    host.set_inline_approvals(hook.clone());

    let res = host
        .call(
            ctx(&[]),
            ActionCall {
                action: "m.touch".into(),
                args: serde_json::Value::Null,
            },
        )
        .await
        .expect("approved memory access should succeed");
    assert_eq!(res.value["v"], "v");

    let perms: Vec<String> = hook
        .seen
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.permission.clone())
        .collect();
    assert!(
        perms.iter().any(|p| p == "memory.write:scratch"),
        "got {perms:?}"
    );
    assert!(
        perms.iter().any(|p| p == "memory.read:scratch"),
        "got {perms:?}"
    );
}

async fn run_action(
    host: &LuaHost,
    grants: &[&str],
    action: &str,
) -> Result<serde_json::Value, String> {
    let mut call_ctx = ctx(grants);
    call_ctx.call_chain = vec![action.into()];
    host.call(
        call_ctx,
        ActionCall {
            action: action.into(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .map(|r| r.value)
    .map_err(|e| e.to_string())
}

/// An ungranted program becomes a question showing the argv and working
/// directory. An allow covers that one invocation, so running the same
/// command again asks again.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn an_ungranted_shell_program_becomes_a_question_per_invocation() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{ name = "s.twice", handler = function(_, ctx)
          local a = ctx.shell("true", { "--one" })
          local b = ctx.shell("true", { "--one" })
          return { a = a.exit_code, b = b.exit_code }
        end }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let hook = StubHook::new(Verdict::AllowOnce);
    host.set_inline_approvals(hook.clone());

    let v = run_action(&host, &[], "s.twice").await.unwrap();
    assert_eq!((v["a"].as_i64(), v["b"].as_i64()), (Some(0), Some(0)));

    let seen = hook.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "asked once per invocation");
    assert_eq!(seen[0].permission, "shell.exec:true");
    let detail = seen[0].detail.as_ref().expect("detail");
    assert_eq!(detail["argv"], serde_json::json!(["true", "--one"]));
}

/// A refused shell question refuses the command, and it never runs.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_shell_question_does_not_run_the_command() {
    let tmp = tempfile::tempdir().unwrap();
    let marker = tmp.path().join("ran");
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{ name = "s.touch", handler = function(_, ctx)
              return ctx.shell("touch", {{ "{}" }})
            end }}
            "#,
            lua_lit(marker.to_string_lossy())
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    host.set_inline_approvals(StubHook::new(Verdict::Deny));
    let err = run_action(&host, &[], "s.touch").await.unwrap_err();
    assert!(err.contains("shell.exec:touch"), "{err}");
    assert!(!marker.exists());
}

/// An approved read covers exactly that request: a second, different file
/// is asked about on its own.
#[tokio::test(flavor = "multi_thread")]
async fn an_allow_covers_only_the_request_that_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.txt");
    let b = tmp.path().join("b.txt");
    std::fs::write(&a, "A").unwrap();
    std::fs::write(&b, "B").unwrap();
    let dir = write_tools(&[(
        "t.lua",
        &format!(
            r#"
            agentd.action{{ name = "r.two", handler = function(_, ctx)
              return {{ a = ctx.fs.read("{}"), b = ctx.fs.read("{}") }}
            end }}
            "#,
            lua_lit(a.to_string_lossy()),
            lua_lit(b.to_string_lossy())
        ),
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let hook = StubHook::new(Verdict::AllowOnce);
    host.set_inline_approvals(hook.clone());
    let v = run_action(&host, &[], "r.two").await.unwrap();
    assert_eq!((v["a"].as_str(), v["b"].as_str()), (Some("A"), Some("B")));
    assert_eq!(hook.seen.lock().unwrap().len(), 2);
}

/// A secret read in the execution is replaced by its name in a question.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_secret_never_appears_in_a_question() {
    let dir = write_tools(&[(
        "t.lua",
        r#"
        agentd.action{ name = "s.leak", handler = function(_, ctx)
          local token = ctx.secret.get("gh_token")
          return ctx.shell("echo", { "Bearer " .. token })
        end }
        "#,
    )]);
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    let store = Arc::new(agentd_secrets::MemoryStore::new());
    agentd_secrets::SecretStore::set(store.as_ref(), "gh_token", "ghp_supersecret").unwrap();
    host.set_secrets(store);
    let hook = StubHook::new(Verdict::AllowOnce);
    host.set_inline_approvals(hook.clone());
    run_action(&host, &["secret:gh_token"], "s.leak")
        .await
        .unwrap();
    let seen = hook.seen.lock().unwrap();
    let detail = seen[0].detail.as_ref().unwrap().to_string();
    assert!(!detail.contains("ghp_supersecret"), "{detail}");
    assert!(detail.contains("<secret gh_token>"), "{detail}");
}

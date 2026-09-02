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

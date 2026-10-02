//! Operator denials hold inside handlers too: a capability or action the
//! deny list forbids is refused even when a grant covers it, and it is never
//! offered for approval.

use std::io::Write;
use std::sync::Arc;

use agentd_permissions::{Caller, PermissionSet};
use agentd_scripting::LuaHost;
use agentd_types::{
    ActionCall, CallContext, Denials, InlineApprovalRequest, InlineApprovals, Registry, Verdict,
};
use async_trait::async_trait;

/// Denies `net:denied.example` and `shell.exec:rm`, and the action `x.secret`.
struct Deny;
impl Denials for Deny {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter(["net:denied.example", "shell.exec:rm"])
    }
    fn denies_action(&self, name: &str) -> bool {
        name == "x.secret"
    }
}

/// Denies every shell command with a bare `shell.exec` denial.
struct DenyAllShell;
impl Denials for DenyAllShell {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter(["shell.exec"])
    }
    fn denies_action(&self, _name: &str) -> bool {
        false
    }
}

/// An approver that would allow anything, to prove denials never reach it.
struct AllowAll;
#[async_trait]
impl InlineApprovals for AllowAll {
    async fn request_inline(&self, _req: InlineApprovalRequest) -> Verdict {
        Verdict::AllowOnce
    }
}

fn host_with(lua: &str) -> (tempfile::TempDir, LuaHost) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::File::create(dir.path().join("t.lua"))
        .unwrap()
        .write_all(lua.as_bytes())
        .unwrap();
    let host = LuaHost::new().unwrap();
    host.load_dir(dir.path()).unwrap();
    host.set_denials(Arc::new(Deny));
    host.set_inline_approvals(Arc::new(AllowAll));
    (dir, host)
}

async fn call(host: &LuaHost, grants: &[&str], action: &str) -> Result<serde_json::Value, String> {
    host.call(
        CallContext {
            caller: Caller::interface("test"),
            effective_grants: PermissionSet::from_iter(grants.iter().copied()),
            call_chain: Vec::new(),
            cwd: None,
        },
        ActionCall {
            action: action.into(),
            args: serde_json::Value::Null,
        },
    )
    .await
    .map(|r| r.value)
    .map_err(|e| e.to_string())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_host_is_refused_under_a_wildcard_grant_and_never_asked() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "h.go", handler = function(_, ctx)
             return ctx.http.get("http://denied.example/") end }"#,
    );
    let err = call(&host, &["net:*"], "h.go").await.unwrap_err();
    assert!(err.contains("net:denied.example"), "{err}");
    assert!(err.contains("denied"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_program_is_refused_under_a_broad_shell_grant() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "s.go", handler = function(_, ctx)
             return ctx.shell("rm", { "-f", "nothing" }) end }"#,
    );
    let err = call(&host, &["shell.exec"], "s.go").await.unwrap_err();
    assert!(err.contains("shell.exec:rm"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_action_is_refused_through_ctx_call() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "x.secret", handler = function() return { ok = true } end }
           agentd.action{ name = "x.outer", handler = function(_, ctx)
             return ctx.call("x.secret", {}) end }"#,
    );
    let err = call(&host, &[], "x.outer").await.unwrap_err();
    assert!(err.contains("x.secret"), "{err}");
    assert!(err.contains("denied"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn granted_calls_that_no_denial_covers_still_work() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "x.inner", handler = function() return { ok = true } end }
           agentd.action{ name = "x.outer", handler = function(_, ctx)
             return ctx.call("x.inner", {}) end }"#,
    );
    assert_eq!(call(&host, &[], "x.outer").await.unwrap()["ok"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bare_shell_denial_refuses_every_program() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "s.go", handler = function(_, ctx)
             return ctx.shell("ls", {}) end }"#,
    );
    host.set_denials(Arc::new(DenyAllShell));
    let err = call(&host, &["shell.exec:ls"], "s.go").await.unwrap_err();
    assert!(err.contains("denied"), "{err}");
}

/// Denies one file, named through a symlinked folder.
struct DenyThroughLink(String);
impl Denials for DenyThroughLink {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter([format!("fs.read:{}", self.0)])
    }
    fn denies_action(&self, _name: &str) -> bool {
        false
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_file_denial_written_through_a_symlink_still_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("secret.txt"), "s").unwrap();
    std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
    let real_file = real
        .join("secret.txt")
        .to_string_lossy()
        .replace('\\', "\\\\");
    let (_d, host) = host_with(&format!(
        r#"agentd.action{{ name = "f.go", handler = function(_, ctx)
             return {{ s = ctx.fs.read("{real_file}") }} end }}"#
    ));
    host.set_denials(Arc::new(DenyThroughLink(
        tmp.path()
            .join("link/secret.txt")
            .to_string_lossy()
            .into_owned(),
    )));
    let grant = format!("fs.read:{}/**", tmp.path().display());
    let err = call(&host, &[&grant], "f.go").await.unwrap_err();
    assert!(err.contains("denied"), "{err}");
}

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

/// Answers every request and declares tools of its own, like the Codex
/// providers.
struct OwnTools;
#[async_trait]
impl agentd_ai::Provider for OwnTools {
    fn name(&self) -> &str {
        "own"
    }
    fn has_own_tools(&self) -> bool {
        true
    }
    async fn complete(
        &self,
        _req: agentd_ai::CompletionRequest,
    ) -> Result<agentd_ai::CompletionResponse, agentd_ai::ProviderError> {
        Ok(agentd_ai::MockProvider::text_only("done"))
    }
}

/// `ctx.ai` refuses a provider with its own tools while a host is denied,
/// and still runs a provider whose tool loop agent.d drives.
#[tokio::test(flavor = "multi_thread")]
async fn ctx_ai_refuses_a_provider_with_its_own_tools_under_a_host_denial() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "a.own", handler = function(_, ctx)
             return ctx.ai.ask("hi", { model = "own/m" }) end }
           agentd.action{ name = "a.plain", handler = function(_, ctx)
             return ctx.ai.ask("hi", { model = "plain/m" }).text end }"#,
    );
    host.set_ai_provider("own", Arc::new(OwnTools));
    host.set_ai_provider(
        "plain",
        Arc::new(agentd_ai::MockProvider::new().with_reply("ok")),
    );
    let err = call(&host, &["ai:own"], "a.own").await.unwrap_err();
    assert!(err.contains("own tools"), "{err}");
    assert!(err.contains("net:denied.example"), "{err}");
    assert_eq!(
        call(&host, &["ai:plain"], "a.plain").await.unwrap(),
        serde_json::json!("ok")
    );
}

/// Denies one program, written as `name` in grants.toml.
struct DenyProgram(&'static str);
impl Denials for DenyProgram {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter([format!("shell.exec:{}", self.0)])
    }
    fn denies_action(&self, _name: &str) -> bool {
        false
    }
}

/// A denial written as a bare name stops the same program asked for by path.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_program_denied_by_name_is_refused_when_asked_for_by_path() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "s.go", handler = function(_, ctx)
             return ctx.shell("/bin/sh", { "-c", "true" }) end }"#,
    );
    host.set_denials(Arc::new(DenyProgram("sh")));
    let err = call(&host, &["shell.exec"], "s.go").await.unwrap_err();
    assert!(err.contains("denied"), "{err}");
}

/// A denial written as a path stops the same program asked for by name.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_program_denied_by_path_is_refused_when_asked_for_by_name() {
    let (_d, host) = host_with(
        r#"agentd.action{ name = "s.go", handler = function(_, ctx)
             return ctx.shell("sh", { "-c", "true" }) end }"#,
    );
    host.set_denials(Arc::new(DenyProgram("/bin/sh")));
    let err = call(&host, &["shell.exec"], "s.go").await.unwrap_err();
    assert!(err.contains("denied"), "{err}");
}

/// Denies reading one file.
struct DenyRead(String);
impl Denials for DenyRead {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter([format!("fs.read:{}", self.0)])
    }
    fn denies_action(&self, _name: &str) -> bool {
        false
    }
}

const RUN_TRUE: &str = r#"agentd.action{ name = "s.go", handler = function(_, ctx)
     return ctx.shell("true") end }"#;

/// A shell call whose granted folder holds a denied file is refused before
/// anything is asked, since its sandbox would let the child read the file.
#[tokio::test(flavor = "multi_thread")]
async fn a_shell_call_whose_folder_holds_a_denied_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("open")).unwrap();
    std::fs::write(root.join(".env"), "k").unwrap();
    let (_d, host) = host_with(RUN_TRUE);
    host.set_denials(Arc::new(DenyRead(
        root.join(".env").to_string_lossy().into_owned(),
    )));

    let wide = format!("fs.read:{}/**", root.display());
    let err = call(&host, &["shell.exec", &wide], "s.go")
        .await
        .unwrap_err();
    assert!(err.contains(".env"), "names the denial: {err}");
    assert!(err.contains("sandbox opens"), "says how: {err}");

    let narrow = format!("fs.read:{}/**", root.join("open").display());
    let res = call(&host, &["shell.exec", &narrow], "s.go").await;
    assert!(
        !res.as_ref().is_err_and(|e| e.contains("sandbox opens")),
        "a folder apart from the denied file is not refused for it: {res:?}"
    );
}

/// An unrestricted shell call reaches every host, so a host denial refuses it.
#[tokio::test(flavor = "multi_thread")]
async fn an_unrestricted_shell_call_is_refused_while_a_host_is_denied() {
    let (_d, host) = host_with(RUN_TRUE);
    let err = call(&host, &["shell.exec", "shell.unrestricted"], "s.go")
        .await
        .unwrap_err();
    assert!(err.contains("net:denied.example"), "{err}");
    assert!(err.contains("unrestricted"), "{err}");
}

/// Denies one host or address.
struct DenyHost(&'static str);
impl Denials for DenyHost {
    fn denied_permissions(&self) -> PermissionSet {
        PermissionSet::from_iter([format!("net:{}", self.0)])
    }
    fn denies_action(&self, _name: &str) -> bool {
        false
    }
}

/// A host grant that covers a denied host is refused; one the denial fully
/// covers is dropped, so the command runs without it.
#[tokio::test(flavor = "multi_thread")]
async fn a_shell_call_whose_host_grant_covers_a_denied_host_is_refused() {
    let (_d, host) = host_with(RUN_TRUE);
    let err = call(&host, &["shell.exec", "net:*"], "s.go")
        .await
        .unwrap_err();
    assert!(err.contains("net:denied.example"), "{err}");
    assert!(err.contains("connect to"), "{err}");

    let res = call(&host, &["shell.exec", "net:denied.example"], "s.go").await;
    assert!(
        !res.as_ref().is_err_and(|e| e.contains("connect to")),
        "a grant the denial fully covers is dropped, not refused: {res:?}"
    );
}

/// A host name may lead to any address, so an address denial refuses a
/// shell call granted a host name.
#[tokio::test(flavor = "multi_thread")]
async fn an_address_denial_refuses_a_shell_call_granted_a_host_name() {
    let (_d, host) = host_with(RUN_TRUE);
    host.set_denials(Arc::new(DenyHost("203.0.113.7")));
    let err = call(&host, &["shell.exec", "net:api.example.com"], "s.go")
        .await
        .unwrap_err();
    assert!(err.contains("net:203.0.113.7"), "{err}");
    assert!(err.contains("api.example.com"), "{err}");
}

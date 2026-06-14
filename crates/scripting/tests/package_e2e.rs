//! End-to-end: a package action is DENIED until the package is trusted, and
//! ALLOWED after. Proves the whole chain — host load -> loaded_packages ->
//! expand_grants -> Engine — and that default-deny holds without trust.

use agentd_packages::expand_grants;
use agentd_permissions::grants::{GrantsFile, PackageGrants};
use agentd_permissions::{ActionMeta, Caller, Decision, Engine, Grants, PermissionSet, ToolMeta};
use agentd_scripting::LuaHost;
use tempfile::tempdir;

fn load_pkg() -> Vec<agentd_packages::LoadedPackage> {
    let pkgroot = tempdir().unwrap();
    let dir = pkgroot.path().join("fix");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.toml"),
        "[package]\nname=\"fix\"\nentry=\"main.lua\"\npermissions=[\"net:example.com\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.lua"),
        r#"
        agentd.tool{ name = "git", requires = { "net:example.com" } }
        agentd.action("git.ping", function(_, ctx) return {} end)
        agentd.runner{ name = "r", actions = { "git.ping" } }
        "#,
    )
    .unwrap();
    let init = pkgroot.path().join("init.lua");
    std::fs::write(&init, r#"import("fix")"#).unwrap();

    let host = LuaHost::new().unwrap();
    host.set_root(pkgroot.path());
    host.set_packages_root(pkgroot.path());
    host.load_file(&init).unwrap();
    host.loaded_packages()
}

// The runner `fix/r` invoking action `fix/git.ping` (tool `fix/git`,
// requires net:example.com).
fn decision_for(grants: GrantsFile) -> Decision {
    let engine = Engine::new(Grants::from_file(grants));
    let tool = ToolMeta {
        name: "fix/git".into(),
        requires: PermissionSet::from_iter(["net:example.com"]),
    };
    let action = ActionMeta {
        name: "fix/git.ping".into(),
        tool: Some("fix/git".into()),
        requires: PermissionSet::from_iter(["net:example.com"]),
        confirm: false,
    };
    let caller = Caller::default().with_runner("fix/r");
    engine.check(&caller, Some(&tool), &action)
}

#[test]
fn untrusted_package_action_is_denied() {
    let pkgs = load_pkg();
    let mut gf = GrantsFile::default();
    // No [package.fix] entry -> expand produces nothing.
    expand_grants(&pkgs, &mut gf);
    assert!(
        matches!(decision_for(gf), Decision::Deny { .. }),
        "untrusted package action must be denied (default-deny)"
    );
}

#[test]
fn trusted_package_action_is_allowed() {
    let pkgs = load_pkg();
    let mut gf = GrantsFile::default();
    gf.package
        .insert("fix".into(), PackageGrants { trusted: true });
    expand_grants(&pkgs, &mut gf);
    assert_eq!(
        decision_for(gf),
        Decision::Allow,
        "trusted package action must be allowed after desugaring"
    );
}

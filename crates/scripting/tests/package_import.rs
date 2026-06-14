use agentd_scripting::LuaHost;
use tempfile::tempdir;

#[test]
fn package_import_prefixes_and_owns_registrations() {
    let pkgroot = tempdir().unwrap();
    let dir = pkgroot.path().join("acme");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("package.toml"),
        "[package]\nname=\"acme\"\nentry=\"main.lua\"\npermissions=[\"net:api.acme.com\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("main.lua"),
        r#"
        agentd.tool{ name = "git", requires = { "net:api.acme.com" } }
        agentd.action("git.diff", function(_, ctx) return {} end)
        agentd.runner{ name = "reviewer", actions = { "git.diff" } }
        "#,
    )
    .unwrap();

    // init.lua imports the package by bare name.
    let init = pkgroot.path().join("init.lua");
    std::fs::write(&init, r#"import("acme")"#).unwrap();

    let host = LuaHost::new().unwrap();
    host.set_root(pkgroot.path()); // any root; package branch ignores it
    host.set_packages_root(pkgroot.path());
    host.load_file(&init).unwrap();

    let pkgs = host.loaded_packages();
    let acme = pkgs.iter().find(|p| p.name == "acme").unwrap();
    assert!(acme.tools.contains(&"acme/git".to_string()));
    assert!(acme.actions.contains(&"acme/git.diff".to_string()));
    assert!(acme.runners.contains(&"acme/reviewer".to_string()));
    assert_eq!(acme.permissions, vec!["net:api.acme.com"]);

    // the runner's allowed action was rewritten to the qualified name
    let r = host.runners().get("acme/reviewer").unwrap();
    assert!(r.allowed_actions.contains(&"acme/git.diff".to_string()));
}

use agentd_permissions::grants::{GrantsFile, RunnerGrants, ServiceGrants, ToolGrants};
use agentd_permissions::model::{Permission, PermissionSet};
use std::path::PathBuf;

/// One package as loaded by the Lua host: its declared perms + everything it
/// registered (fully-qualified, already prefixed with the package name).
#[derive(Debug, Clone)]
pub struct LoadedPackage {
    pub name: String,
    pub permissions: Vec<String>,
    pub tools: Vec<String>,
    pub actions: Vec<String>,
    pub runners: Vec<String>,
    pub services: Vec<String>,
}

/// Desugar every TRUSTED package into the grant rows the 5-layer engine
/// enforces. Only fills slots the user left absent — an explicit `grants.toml`
/// entry for a component always wins (so a single component can be narrowed).
pub fn expand_grants(packages: &[LoadedPackage], grants: &mut GrantsFile) {
    for pkg in packages {
        let trusted = grants
            .package
            .get(&pkg.name)
            .map(|p| p.trusted)
            .unwrap_or(false);
        if !trusted {
            continue;
        }
        let perms = PermissionSet(
            pkg.permissions
                .iter()
                .map(|p| Permission::new(expand_tilde_slug(p)))
                .collect(),
        );

        for tool in &pkg.tools {
            if !grants.tool.contains_key(tool) {
                grants.tool.insert(
                    tool.clone(),
                    ToolGrants {
                        granted: perms.clone(),
                    },
                );
            }
        }
        for runner in &pkg.runners {
            if !grants.runner.contains_key(runner) {
                grants.runner.insert(
                    runner.clone(),
                    RunnerGrants {
                        allowed_actions: pkg.actions.iter().cloned().collect(),
                        granted: PermissionSet::empty(),
                    },
                );
            }
        }
        for svc in &pkg.services {
            if !grants.service.contains_key(svc) {
                grants.service.insert(
                    svc.clone(),
                    ServiceGrants {
                        granted: perms.clone(),
                        allowed_actions: pkg.actions.iter().cloned().collect(),
                    },
                );
            }
        }
    }
}

/// Expand a leading `~/` to the user's home directory. Slugs without `~/`
/// pass through unchanged.
pub fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

/// Expand `~/` inside a permission slug's specifier
/// (`fs.write:~/x` -> `fs.write:/home/me/x`). Resolution must happen before the
/// engine, which matches `fs.*:<abs-path>` literally.
fn expand_tilde_slug(slug: &str) -> String {
    match slug.split_once(':') {
        Some((domain, spec)) if spec.starts_with("~/") => {
            format!("{domain}:{}", expand_tilde(spec).display())
        }
        _ => slug.to_string(),
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_permissions::grants::{GrantsFile, PackageGrants};

    fn pkg() -> LoadedPackage {
        LoadedPackage {
            name: "acme".into(),
            permissions: vec!["net:api.acme.com".into()],
            tools: vec!["acme/git".into()],
            actions: vec!["acme/git.diff".into(), "acme/git.status".into()],
            runners: vec!["acme/reviewer".into()],
            services: vec!["acme/poller".into()],
        }
    }

    #[test]
    fn trusted_package_expands_into_grant_rows() {
        let mut gf = GrantsFile::default();
        gf.package
            .insert("acme".into(), PackageGrants { trusted: true });

        expand_grants(&[pkg()], &mut gf);

        let t = gf.tool.get("acme/git").unwrap();
        assert!(t.granted.0.iter().any(|p| p.as_str() == "net:api.acme.com"));
        let r = gf.runner.get("acme/reviewer").unwrap();
        assert!(r.allowed_actions.contains("acme/git.diff"));
        assert!(r.allowed_actions.contains("acme/git.status"));
        let s = gf.service.get("acme/poller").unwrap();
        assert!(s.granted.0.iter().any(|p| p.as_str() == "net:api.acme.com"));
        assert!(s.allowed_actions.contains("acme/git.diff"));
    }

    #[test]
    fn untrusted_package_expands_nothing() {
        let mut gf = GrantsFile::default();
        expand_grants(&[pkg()], &mut gf);
        assert!(gf.tool.is_empty());
        assert!(gf.runner.is_empty());
    }

    #[test]
    fn explicit_user_entry_wins() {
        let mut gf = GrantsFile::default();
        gf.package
            .insert("acme".into(), PackageGrants { trusted: true });
        gf.tool.insert("acme/git".into(), Default::default());

        expand_grants(&[pkg()], &mut gf);

        let t = gf.tool.get("acme/git").unwrap();
        assert!(t.granted.0.is_empty());
    }

    #[test]
    fn tilde_in_perms_is_expanded() {
        let mut p = pkg();
        p.permissions = vec!["fs.write:~/x/**".into()];
        let mut gf = GrantsFile::default();
        gf.package
            .insert("acme".into(), PackageGrants { trusted: true });
        expand_grants(&[p], &mut gf);
        let t = gf.tool.get("acme/git").unwrap();
        assert!(t.granted.0.iter().all(|p| !p.as_str().contains('~')));
    }
}

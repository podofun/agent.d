//! Grant store. Backed by a TOML file at `$XDG_CONFIG_HOME/agentd/grants.toml`.
//!
//! Shape:
//!
//! ```toml
//! [tool.google_calendar]
//! granted = ["net:googleapis.com", "oauth:google"]
//!
//! [runner.backend_reviewer]
//! allowed_actions = ["git.diff", "github.read_pr", "github.comment_pr"]
//! granted = []
//!
//! [interface.telegram]
//! allowed_actions = ["git.status"]
//! granted = []
//!
//! [denial]
//! actions = []
//! permissions = []
//!
//! [approval]
//! skip = []
//! ```
//!
//! Every table rejects unknown keys, so a misspelled section or key fails to
//! load instead of silently dropping a rule.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Approval, ApprovalSetting, Denial, Permission, PermissionSet};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolGrants {
    #[serde(default)]
    pub granted: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerGrants {
    #[serde(default)]
    pub allowed_actions: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub granted: PermissionSet,
    /// Who answers this runner's questions by default. `None` is `human`.
    /// It lives here, never in Lua, so a runner cannot choose its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalSetting>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InterfaceGrants {
    #[serde(default)]
    pub allowed_actions: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub granted: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceGrants {
    /// Permission slugs the service can exercise via `agentd.context.*`.
    #[serde(default)]
    pub granted: PermissionSet,
    /// Optional allowlist if the service dispatches actions via
    /// `agentd.context.tools.call(...)`. Empty = no constraint at this layer.
    #[serde(default)]
    pub allowed_actions: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageGrants {
    /// Single approval that inherits to every component the package registers.
    /// The loader desugars a trusted package into per-tool/runner/service rows;
    /// the engine never reads this field directly.
    #[serde(default)]
    pub trusted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantsFile {
    #[serde(default)]
    pub tool: BTreeMap<String, ToolGrants>,
    #[serde(default)]
    pub runner: BTreeMap<String, RunnerGrants>,
    #[serde(default)]
    pub interface: BTreeMap<String, InterfaceGrants>,
    #[serde(default)]
    pub service: BTreeMap<String, ServiceGrants>,
    #[serde(default)]
    pub package: BTreeMap<String, PackageGrants>,
    #[serde(default)]
    pub denial: Denial,
    #[serde(default)]
    pub approval: Approval,
    #[serde(default)]
    pub folders: Folders,
}

/// `[folders]`: the folders chat sessions may work in without asking.
/// "Always trust" on a folder question appends here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Folders {
    /// Absolute folders or globs (`/home/me/code/**`); `~` means the home folder.
    #[serde(default)]
    pub trusted: Vec<String>,
}

/// The user's home folder, for `~` in trusted folders.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn expand_home(entry: &str) -> String {
    match (entry.strip_prefix("~/").or(entry.strip_prefix("~")), home_dir()) {
        (Some(rest), Some(home)) if entry == "~" || entry.starts_with("~/") => {
            home.join(rest).to_string_lossy().into_owned()
        }
        _ => entry.to_owned(),
    }
}

/// Materialized grants used by the engine. Provides fast lookups.
#[derive(Debug, Clone, Default)]
pub struct Grants {
    pub file: GrantsFile,
}

impl Grants {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn from_file(file: GrantsFile) -> Self {
        Self { file }
    }

    pub fn tool(&self, name: &str) -> Option<&ToolGrants> {
        self.file.tool.get(name)
    }
    pub fn runner(&self, name: &str) -> Option<&RunnerGrants> {
        self.file.runner.get(name)
    }
    pub fn interface(&self, name: &str) -> Option<&InterfaceGrants> {
        self.file.interface.get(name)
    }
    pub fn service(&self, name: &str) -> Option<&ServiceGrants> {
        self.file.service.get(name)
    }
    pub fn package(&self, name: &str) -> Option<&PackageGrants> {
        self.file.package.get(name)
    }
    /// Whether `folder` (an absolute real path) is one the user trusts,
    /// matched like an `fs.read` grant so globs mean the same thing.
    pub fn trusts_folder(&self, folder: &Path) -> bool {
        let wanted = Permission::new(format!("fs.read:{}", folder.display()));
        self.file.folders.trusted.iter().any(|entry| {
            Permission::new(format!("fs.read:{}", expand_home(entry))).covers(&wanted)
        })
    }

    pub fn denial(&self) -> &Denial {
        &self.file.denial
    }
    pub fn approval(&self) -> &Approval {
        &self.file.approval
    }
}

pub fn load_grants_file(path: &Path) -> Result<GrantsFile, String> {
    if !path.exists() {
        return Ok(GrantsFile::default());
    }
    let body = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "could not read the grants file `{}` ({e}). Check that the file exists and is readable",
            path.display()
        )
    })?;
    parse_grants(&body).map_err(|e| format!("the grants file `{}` {e}", path.display()))
}

/// Parse and check grants.toml text. Errors read as the end of a sentence
/// that starts with the file's name.
pub fn parse_grants(body: &str) -> Result<GrantsFile, String> {
    let table: toml::Table = toml::from_str(body)
        .map_err(|e| format!("is not valid TOML ({e}). Fix the syntax and try again"))?;
    if table.contains_key("policy") {
        return Err("still has a `[policy]` section, which was replaced. Move `deny_actions` to `[denial].actions`, `deny_permissions` to `[denial].permissions`, and `auto_confirm` to `[approval].skip`".into());
    }
    for kind in ["tool", "service", "interface", "package"] {
        if let Some(name) = table
            .get(kind)
            .and_then(|v| v.as_table())
            .and_then(|t| {
                t.iter()
                    .find(|(_, v)| v.as_table().is_some_and(|e| e.contains_key("approval")))
            })
            .map(|(name, _)| name)
        {
            return Err(format!(
                "sets `approval` in `[{kind}.{name}]`, but approval is set per runner only. Move it to a `[runner.<name>]` section"
            ));
        }
    }
    let file: GrantsFile = table.try_into().map_err(|e| {
        format!("has an entry agentd does not know ({e}). Check the spelling of the section and key names")
    })?;
    check_grants(&file)?;
    Ok(file)
}

/// Reject a rule that could never match: a permission pattern the matcher
/// does not support, or a file denial that is not an absolute path.
fn check_grants(file: &GrantsFile) -> Result<(), String> {
    let sets = file
        .tool
        .iter()
        .map(|(n, g)| (format!("[tool.{n}]"), &g.granted))
        .chain(
            file.runner
                .iter()
                .map(|(n, g)| (format!("[runner.{n}]"), &g.granted)),
        )
        .chain(
            file.interface
                .iter()
                .map(|(n, g)| (format!("[interface.{n}]"), &g.granted)),
        )
        .chain(
            file.service
                .iter()
                .map(|(n, g)| (format!("[service.{n}]"), &g.granted)),
        )
        .chain(std::iter::once((
            "[denial]".to_string(),
            &file.denial.permissions,
        )));
    for (section, set) in sets {
        for p in set.iter() {
            if let Some(why) = unsupported_pattern(p) {
                return Err(format!(
                    "lists `{}` in `{section}`, which can never match because {why}",
                    p.as_str()
                ));
            }
        }
    }
    for p in file.denial.permissions.iter() {
        if p.as_str().contains("{cwd}") {
            return Err(format!(
                "uses `{{cwd}}` in `[denial]` (`{}`), but denials must be absolute paths that hold for every session",
                p.as_str()
            ));
        }
    }
    for entry in &file.folders.trusted {
        let expanded = expand_home(entry);
        if !is_absolute_path(&expanded) {
            return Err(format!(
                "trusts the folder `{entry}` in `[folders]`, which is not an absolute path. Write the full path, or start it with `~/`"
            ));
        }
        let trimmed = expanded.trim_end_matches(['*', '/', '\\']);
        if trimmed.is_empty() || Path::new(&expanded).parent().is_none() || trimmed.ends_with(':') {
            return Err(format!(
                "trusts `{entry}` in `[folders]`, which is the root of the disk. Trust the folders you work in instead"
            ));
        }
    }
    for p in file.denial.permissions.iter() {
        if let ("fs.read" | "fs.write", Some(path)) = p.parts()
            && !is_absolute_path(path)
        {
            return Err(format!(
                "denies `{}`, which is not an absolute path, so it could deny nothing. Write the full path, starting from the root",
                p.as_str()
            ));
        }
    }
    Ok(())
}

/// Why the matcher can never match `p`, if it cannot. A `*` is supported only
/// at the very end (`api.*`, `/dir/*`, `/dir/**`), and IPv6 addresses are
/// compared in their shortest form, so a prefix of one may never match.
fn unsupported_pattern(p: &Permission) -> Option<&'static str> {
    let (domain, spec) = p.parts();
    if domain.contains('*') {
        return Some("a `*` is only supported at the end of the part after the colon");
    }
    let spec = spec?;
    let body = spec.trim_end_matches('*');
    if body.contains('*') {
        return Some("a `*` is only supported at the end, as in `api.*` or `/dir/**`");
    }
    if domain == "net" && spec.ends_with('*') && body.contains(':') {
        return Some(
            "IPv6 addresses are compared in their shortest form, so grant or deny whole addresses",
        );
    }
    None
}

fn is_absolute_path(path: &str) -> bool {
    !path.starts_with('~')
        && (Path::new(path).is_absolute() || path.starts_with('/') || path.starts_with('\\'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_grants_file() {
        let toml = r#"
            [tool.google_calendar]
            granted = ["net:googleapis.com", "oauth:google"]

            [runner.backend_reviewer]
            allowed_actions = ["git.diff", "github.comment_pr"]
            granted = []

            [interface.telegram]
            allowed_actions = ["git.status"]

            [denial]
            actions = ["shell.exec"]
            permissions = ["fs.read:/proj/.env"]

            [approval]
            skip = ["deploy.preview"]
        "#;
        let f = parse_grants(toml).unwrap();
        assert_eq!(f.tool.len(), 1);
        let tool = f.tool.get("google_calendar").unwrap();
        assert!(tool.granted.0.iter().any(|p| p.as_str() == "oauth:google"));
        let runner = f.runner.get("backend_reviewer").unwrap();
        assert!(runner.allowed_actions.contains("git.diff"));
        assert!(f.denial.actions.contains("shell.exec"));
        assert!(f.approval.skips("deploy.preview"));
        let _g = Grants::from_file(f);
    }

    fn err(toml: &str) -> String {
        parse_grants(toml).unwrap_err()
    }

    #[test]
    fn a_leftover_policy_section_names_the_new_sections() {
        let e = err("[policy]\ndeny_actions = [\"x\"]\n");
        assert!(e.contains("[denial].actions"), "{e}");
        assert!(e.contains("[approval].skip"), "{e}");
    }

    #[test]
    fn misspelled_sections_and_keys_fail_to_load() {
        assert!(err("[denail]\nactions = []\n").contains("denail"));
        assert!(err("[denial]\naction = []\n").contains("action"));
        assert!(err("[tool.t]\ngrant = []\n").contains("grant"));
        assert!(err("[approval]\nskips = []\n").contains("skips"));
    }

    #[test]
    fn unsupported_patterns_are_rejected() {
        let e = err("[denial]\npermissions = [\"net:*.example.com\"]\n");
        assert!(
            e.contains("net:*.example.com") && e.contains("[denial]"),
            "{e}"
        );
        assert!(err("[tool.t]\ngranted = [\"fs.read:/a/*/b\"]\n").contains("[tool.t]"));
        assert!(err("[tool.t]\ngranted = [\"fs.*\"]\n").contains("fs.*"));
        assert!(err("[tool.t]\ngranted = [\"net:2001:db8:*\"]\n").contains("IPv6"));
        for ok in [
            "net:api.*",
            "fs.read:/dir/**",
            "fs.read:/dir/*",
            "net:169.254.*",
            "net:*",
            "shell.exec",
        ] {
            parse_grants(&format!("[tool.t]\ngranted = [\"{ok}\"]\n")).unwrap();
        }
    }

    #[test]
    fn file_denials_must_be_absolute() {
        assert!(err("[denial]\npermissions = [\"fs.read:.env\"]\n").contains("absolute"));
        assert!(err("[denial]\npermissions = [\"fs.write:~/secrets\"]\n").contains("absolute"));
        parse_grants("[denial]\npermissions = [\"fs.read:/proj/.env\", \"fs.read\"]\n").unwrap();
    }

    #[test]
    fn trusted_folders_match_paths_and_globs() {
        let grants = Grants::from_file(
            parse_grants("[folders]\ntrusted = [\"/w/a\", \"/w/code/**\"]\n").unwrap(),
        );
        assert!(grants.trusts_folder(Path::new("/w/a")));
        assert!(grants.trusts_folder(Path::new("/w/code/x/y")));
        assert!(!grants.trusts_folder(Path::new("/w/b")));
        assert!(!grants.trusts_folder(Path::new("/w/a/inner")));
    }

    #[test]
    fn trusted_folders_expand_the_home_folder() {
        let home = home_dir().expect("a home folder in the test environment");
        let grants = Grants::from_file(parse_grants("[folders]\ntrusted = [\"~/code/**\"]\n").unwrap());
        assert!(grants.trusts_folder(&home.join("code").join("x")));
    }

    #[test]
    fn bad_trusted_entries_are_load_errors() {
        assert!(err("[folders]\ntrusted = [\"rel/path\"]\n").contains("absolute"));
        assert!(err("[folders]\ntrusted = [\"/\"]\n").contains("root"));
    }

    #[test]
    fn cwd_placeholder_is_refused_in_denials() {
        let e = err("[denial]\npermissions = [\"fs.write:{cwd}/**\"]\n");
        assert!(e.contains("{cwd}") && e.contains("[denial]"), "{e}");
        parse_grants("[tool.files]\ngranted = [\"fs.write:{cwd}/**\"]\n").unwrap();
    }

    #[test]
    fn approval_is_a_runner_setting_only() {
        let f = parse_grants("[runner.coder]\napproval = \"nobody\"\n").unwrap();
        assert_eq!(f.runner["coder"].approval, Some(ApprovalSetting::Nobody));
        let e = err("[tool.git]\napproval = \"nobody\"\n");
        assert!(e.contains("[tool.git]") && e.contains("per runner"), "{e}");
        assert!(err("[runner.coder]\napproval = \"sometimes\"\n").contains("sometimes"));
    }

    #[test]
    fn missing_file_yields_default() {
        let p = std::path::PathBuf::from("/nonexistent/agentd-grants.toml");
        let f = load_grants_file(&p).unwrap();
        assert!(f.tool.is_empty());
        assert!(f.runner.is_empty());
    }
}

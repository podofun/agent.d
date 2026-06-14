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
//! [policy]
//! deny_actions = []
//! deny_permissions = []
//! ```

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{PermissionSet, Policy};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolGrants {
    #[serde(default)]
    pub granted: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunnerGrants {
    #[serde(default)]
    pub allowed_actions: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub granted: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InterfaceGrants {
    #[serde(default)]
    pub allowed_actions: std::collections::BTreeSet<String>,
    #[serde(default)]
    pub granted: PermissionSet,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
pub struct PackageGrants {
    /// Single approval that inherits to every component the package registers.
    /// The loader desugars a trusted package into per-tool/runner/service rows;
    /// the engine never reads this field directly.
    #[serde(default)]
    pub trusted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    pub policy: Policy,
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
    pub fn policy(&self) -> &Policy {
        &self.file.policy
    }
}

pub fn load_grants_file(path: &Path) -> Result<GrantsFile, String> {
    if !path.exists() {
        return Ok(GrantsFile::default());
    }
    let body =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    toml::from_str(&body).map_err(|e| format!("parse {}: {e}", path.display()))
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

            [policy]
            deny_actions = ["shell.exec"]
        "#;
        let f: GrantsFile = toml::from_str(toml).unwrap();
        assert_eq!(f.tool.len(), 1);
        let tool = f.tool.get("google_calendar").unwrap();
        assert!(tool.granted.0.iter().any(|p| p.as_str() == "oauth:google"));
        let runner = f.runner.get("backend_reviewer").unwrap();
        assert!(runner.allowed_actions.contains("git.diff"));
        assert!(f.policy.deny_actions.contains("shell.exec"));
        let _g = Grants::from_file(f);
    }

    #[test]
    fn missing_file_yields_default() {
        let p = std::path::PathBuf::from("/nonexistent/agentd-grants.toml");
        let f = load_grants_file(&p).unwrap();
        assert!(f.tool.is_empty());
        assert!(f.runner.is_empty());
    }
}

use serde::Deserialize;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("read {0}: {1}")]
    Read(String, std::io::Error),
    #[error("parse {0}: {1}")]
    Parse(String, toml::de::Error),
}

/// Parsed `package.toml`. Declares what the package WANTS — never self-grants.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub version: Option<String>,
    pub entry: String,
    pub permissions: Vec<String>,
}

#[derive(Deserialize)]
struct RawFile {
    package: RawPackage,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    version: Option<String>,
    entry: Option<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

impl Manifest {
    pub fn parse(src: &str) -> Result<Self, ManifestError> {
        let raw: RawFile =
            toml::from_str(src).map_err(|e| ManifestError::Parse("package.toml".into(), e))?;
        Ok(Self {
            name: raw.package.name,
            version: raw.package.version,
            entry: raw.package.entry.unwrap_or_else(|| "main.lua".to_string()),
            permissions: raw.package.permissions,
        })
    }

    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let body = std::fs::read_to_string(path)
            .map_err(|e| ManifestError::Read(path.display().to_string(), e))?;
        Self::parse(&body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_manifest() {
        let src = r#"
            [package]
            name = "acme"
            version = "1.2.0"
            entry = "main.lua"
            permissions = ["net:api.acme.com", "shell.exec:git"]
        "#;
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.name, "acme");
        assert_eq!(m.entry, "main.lua");
        assert_eq!(m.permissions, vec!["net:api.acme.com", "shell.exec:git"]);
    }

    #[test]
    fn entry_defaults_to_main_lua() {
        let src = r#"
            [package]
            name = "acme"
        "#;
        let m = Manifest::parse(src).unwrap();
        assert_eq!(m.entry, "main.lua");
        assert!(m.permissions.is_empty());
    }

    #[test]
    fn missing_name_is_error() {
        let src = "[package]\nentry = \"main.lua\"\n";
        assert!(Manifest::parse(src).is_err());
    }
}

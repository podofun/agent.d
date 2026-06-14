use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// One installed package's provenance + pin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    #[serde(skip)]
    pub name: String,
    pub url: String,
    // `ref` is a reserved keyword — raw identifier, serialized as "ref".
    #[serde(rename = "ref")]
    pub r#ref: String,
    pub commit: String,
}

/// The whole `~/.local/share/agentd/packages/index.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PackageIndex {
    entries: BTreeMap<String, IndexEntry>,
}

impl PackageIndex {
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(path).map_err(|e| format!("read index: {e}"))?;
        let mut idx: PackageIndex =
            toml::from_str(&body).map_err(|e| format!("parse index: {e}"))?;
        // `name` is skipped during (de)serialize; backfill from the map key.
        for (k, v) in idx.entries.iter_mut() {
            v.name = k.clone();
        }
        Ok(idx)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
        }
        let body = toml::to_string_pretty(self).map_err(|e| format!("encode index: {e}"))?;
        std::fs::write(path, body).map_err(|e| format!("write index: {e}"))
    }

    pub fn get(&self, name: &str) -> Option<&IndexEntry> {
        self.entries.get(name)
    }

    pub fn set(&mut self, entry: IndexEntry) {
        self.entries.insert(entry.name.clone(), entry);
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = &IndexEntry> {
        self.entries.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrips_through_disk() {
        let td = tempdir().unwrap();
        let path = td.path().join("index.toml");

        let mut idx = PackageIndex::default();
        idx.set(IndexEntry {
            name: "acme".into(),
            url: "https://example.com/acme.git".into(),
            r#ref: "v1.2".into(),
            commit: "abc123".into(),
        });
        idx.save(&path).unwrap();

        let loaded = PackageIndex::load(&path).unwrap();
        let e = loaded.get("acme").unwrap();
        assert_eq!(e.commit, "abc123");
        assert_eq!(e.url, "https://example.com/acme.git");
        assert_eq!(e.name, "acme");
    }

    #[test]
    fn missing_file_is_empty() {
        let idx = PackageIndex::load(std::path::Path::new("/nope/index.toml")).unwrap();
        assert!(idx.get("acme").is_none());
    }
}

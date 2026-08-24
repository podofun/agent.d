//! Secrets store.
//!
//! Backends:
//! - `MemoryStore`  — process-only, zeroized on drop. Tests + ephemeral use.
//! - `KeyringStore` — OS-native keyring (libsecret / Keychain / Cred Manager)
//!   via `keyring-core` + platform store crate. Installs the platform default
//!   store on first use, idempotently.
//! - `EnvStore`     — read-only, `AGENTD_SECRET_<KEY>` environment variables.
//!   For containers where the platform injects secrets into the environment.
//! - `DirStore`     — read-only, one file per secret under a directory. Matches
//!   Docker secrets (`/run/secrets`) and Kubernetes secret volume mounts.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use thiserror::Error;
use zeroize::Zeroize;

pub const DEFAULT_SERVICE: &str = "agentd";

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("no secret named `{0}` is stored")]
    NotFound(String),
    #[error("the secrets backend reported an error ({0})")]
    Backend(String),
}

pub type Result<T> = std::result::Result<T, SecretError>;

pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<String>;
    fn try_get(&self, key: &str) -> Result<Option<String>> {
        match self.get(key) {
            Ok(v) => Ok(Some(v)),
            Err(SecretError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
    fn list(&self) -> Result<Vec<String>>;
}

// ---------- MemoryStore ----------

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<HashMap<String, String>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Drop for MemoryStore {
    fn drop(&mut self) {
        if let Ok(mut map) = self.inner.lock() {
            for (_, mut v) in map.drain() {
                v.zeroize();
            }
        }
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<String> {
        let map = self
            .inner
            .lock()
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        map.get(key)
            .cloned()
            .ok_or_else(|| SecretError::NotFound(key.to_string()))
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        map.insert(key.to_string(), value.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        let mut map = self
            .inner
            .lock()
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        match map.remove(key) {
            Some(mut v) => {
                v.zeroize();
                Ok(())
            }
            None => Err(SecretError::NotFound(key.to_string())),
        }
    }
    fn list(&self) -> Result<Vec<String>> {
        let map = self
            .inner
            .lock()
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        Ok(keys)
    }
}

static STORE_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

fn ensure_default_store() -> Result<()> {
    let res = STORE_INIT.get_or_init(install_default_store);
    res.clone().map_err(SecretError::Backend)
}

#[cfg(target_os = "linux")]
fn install_default_store() -> std::result::Result<(), String> {
    use zbus_secret_service_keyring_store::Store;
    let store = Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "macos")]
fn install_default_store() -> std::result::Result<(), String> {
    use apple_native_keyring_store::keychain::Store;
    let store = Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_default_store() -> std::result::Result<(), String> {
    use windows_native_keyring_store::Store;
    let store = Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn install_default_store() -> std::result::Result<(), String> {
    Err("no keyring store available for this platform".into())
}

pub struct KeyringStore {
    service: String,
    /// Tracks keys set through this process. OS keyrings expose no portable
    /// enumeration; cross-process keys set elsewhere will NOT be listed.
    index: Mutex<BTreeSet<String>>,
}

impl KeyringStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            index: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn default_service() -> Self {
        Self::new(DEFAULT_SERVICE)
    }

    fn entry(&self, key: &str) -> Result<keyring_core::Entry> {
        ensure_default_store()?;
        keyring_core::Entry::new(&self.service, key)
            .map_err(|e| SecretError::Backend(e.to_string()))
    }

    fn map_keyring_err(e: keyring_core::Error, key: &str) -> SecretError {
        if matches!(e, keyring_core::Error::NoEntry) {
            SecretError::NotFound(key.to_string())
        } else {
            SecretError::Backend(e.to_string())
        }
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &str) -> Result<String> {
        let entry = self.entry(key)?;
        entry
            .get_password()
            .map_err(|e| Self::map_keyring_err(e, key))
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        let entry = self.entry(key)?;
        entry
            .set_password(value)
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        let mut idx = self
            .index
            .lock()
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        idx.insert(key.to_string());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        let entry = self.entry(key)?;
        entry
            .delete_credential()
            .map_err(|e| Self::map_keyring_err(e, key))?;
        let mut idx = self
            .index
            .lock()
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        idx.remove(key);
        Ok(())
    }
    fn list(&self) -> Result<Vec<String>> {
        let idx = self
            .index
            .lock()
            .map_err(|e| SecretError::Backend(e.to_string()))?;
        Ok(idx.iter().cloned().collect())
    }
}

// ---------- EnvStore ----------

pub const ENV_PREFIX: &str = "AGENTD_SECRET_";

/// Read-only store backed by `AGENTD_SECRET_<KEY>` environment variables.
/// The key is uppercased and dashes become underscores: the secret
/// `github-webhook` reads `AGENTD_SECRET_GITHUB_WEBHOOK`.
#[derive(Default)]
pub struct EnvStore;

impl EnvStore {
    pub fn new() -> Self {
        Self
    }

    fn var_name(key: &str) -> String {
        let mapped: String = key
            .chars()
            .map(|c| {
                if c == '-' {
                    '_'
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect();
        format!("{ENV_PREFIX}{mapped}")
    }
}

impl SecretStore for EnvStore {
    fn get(&self, key: &str) -> Result<String> {
        std::env::var(Self::var_name(key)).map_err(|_| SecretError::NotFound(key.to_string()))
    }
    fn set(&self, _key: &str, _value: &str) -> Result<()> {
        Err(SecretError::Backend(
            "the env secret store is read-only — secrets are provided by the environment".into(),
        ))
    }
    fn delete(&self, _key: &str) -> Result<()> {
        Err(SecretError::Backend(
            "the env secret store is read-only — secrets are provided by the environment".into(),
        ))
    }
    fn list(&self) -> Result<Vec<String>> {
        let mut keys: Vec<String> = std::env::vars()
            .filter_map(|(name, _)| {
                name.strip_prefix(ENV_PREFIX)
                    .map(|k| k.to_ascii_lowercase())
            })
            .collect();
        keys.sort();
        Ok(keys)
    }
}

// ---------- DirStore ----------

/// Read-only store with one file per secret under a directory, the shape of
/// Docker secrets (`/run/secrets`) and Kubernetes secret volume mounts.
/// A single trailing newline is trimmed from each value.
pub struct DirStore {
    root: std::path::PathBuf,
}

impl DirStore {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, key: &str) -> Result<std::path::PathBuf> {
        let valid = !key.is_empty()
            && key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
            && !key.starts_with('.');
        if !valid {
            return Err(SecretError::Backend(format!(
                "the secret name `{key}` is not a plain file name"
            )));
        }
        Ok(self.root.join(key))
    }
}

impl SecretStore for DirStore {
    fn get(&self, key: &str) -> Result<String> {
        let path = self.path_for(key)?;
        match std::fs::read_to_string(&path) {
            Ok(mut v) => {
                if v.ends_with('\n') {
                    v.pop();
                    if v.ends_with('\r') {
                        v.pop();
                    }
                }
                Ok(v)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(SecretError::NotFound(key.to_string()))
            }
            Err(e) => Err(SecretError::Backend(format!(
                "could not read the secret file {} ({e})",
                path.display()
            ))),
        }
    }
    fn set(&self, _key: &str, _value: &str) -> Result<()> {
        Err(SecretError::Backend(
            "the dir secret store is read-only — secrets are mounted files".into(),
        ))
    }
    fn delete(&self, _key: &str) -> Result<()> {
        Err(SecretError::Backend(
            "the dir secret store is read-only — secrets are mounted files".into(),
        ))
    }
    fn list(&self) -> Result<Vec<String>> {
        let entries = std::fs::read_dir(&self.root).map_err(|e| {
            SecretError::Backend(format!(
                "could not list the secrets directory {} ({e})",
                self.root.display()
            ))
        })?;
        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| SecretError::Backend(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            // metadata() follows symlinks: Kubernetes mounts each key as a
            // symlink into a `..data` directory.
            let is_file = std::fs::metadata(entry.path())
                .map(|m| m.is_file())
                .unwrap_or(false);
            if is_file && !name.starts_with('.') {
                keys.push(name);
            }
        }
        keys.sort();
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_set_get_delete_list() {
        let s = MemoryStore::new();
        assert!(matches!(s.get("k"), Err(SecretError::NotFound(_))));
        s.set("k", "v").unwrap();
        assert_eq!(s.get("k").unwrap(), "v");
        assert_eq!(s.try_get("k").unwrap().as_deref(), Some("v"));
        assert_eq!(s.try_get("missing").unwrap(), None);
        s.set("a", "1").unwrap();
        assert_eq!(s.list().unwrap(), vec!["a".to_string(), "k".to_string()]);
        s.delete("k").unwrap();
        assert!(s.try_get("k").unwrap().is_none());
        assert!(matches!(s.delete("k"), Err(SecretError::NotFound(_))));
    }

    #[test]
    fn memory_overwrite() {
        let s = MemoryStore::new();
        s.set("k", "v1").unwrap();
        s.set("k", "v2").unwrap();
        assert_eq!(s.get("k").unwrap(), "v2");
    }
}

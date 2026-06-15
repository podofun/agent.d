//! Secrets store.
//!
//! Backends:
//! - `MemoryStore`  — process-only, zeroized on drop. Tests + ephemeral use.
//! - `KeyringStore` — OS-native keyring (libsecret / Keychain / Cred Manager)
//!   via `keyring-core` + platform store crate. Installs the platform default
//!   store on first use, idempotently.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};

use thiserror::Error;
use zeroize::Zeroize;

pub const DEFAULT_SERVICE: &str = "agentd";

#[derive(Debug, Error)]
pub enum SecretError {
    #[error("secret `{0}` not found")]
    NotFound(String),
    #[error("backend error: {0}")]
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

//! Durable namespaced key/value store backing Lua `ctx.memory`.
//!
//! Values are opaque bytes (the scripting layer puts JSON in them). Addressed
//! by `(namespace, key)`. `MemMemoryStore` is the in-process test double;
//! `RedbStore` is the production impl.

mod redb_store;
pub use redb_store::RedbStore;

use std::collections::BTreeMap;
use std::sync::RwLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    // Rendered under caller-supplied context ("could not open the memory
    // database ..."), so no extra prefix here.
    #[error("{0}")]
    Backend(String),
    #[error("invalid memory key — {0}")]
    InvalidKey(String),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

/// Reject a NUL byte in `ns`/`key` — the composite key uses NUL as separator.
pub(crate) fn check_no_nul(label: &str, s: &str) -> Result<()> {
    if s.as_bytes().contains(&0) {
        return Err(MemoryError::InvalidKey(format!("{label} contains NUL")));
    }
    Ok(())
}

pub trait MemoryStore: Send + Sync {
    fn get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, ns: &str, key: &str, value: &[u8]) -> Result<()>;
    fn delete(&self, ns: &str, key: &str) -> Result<bool>;
    fn exists(&self, ns: &str, key: &str) -> Result<bool>;
    fn keys(&self, ns: &str) -> Result<Vec<String>>;
    fn clear(&self, ns: &str) -> Result<()>;
}

/// In-process store for tests. Not durable.
#[derive(Default)]
pub struct MemMemoryStore {
    // (ns, key) -> value
    inner: RwLock<BTreeMap<(String, String), Vec<u8>>>,
}

impl MemMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl MemoryStore for MemMemoryStore {
    fn get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>> {
        check_no_nul("ns", ns)?;
        check_no_nul("key", key)?;
        Ok(self
            .inner
            .read()
            .unwrap()
            .get(&(ns.to_string(), key.to_string()))
            .cloned())
    }
    fn set(&self, ns: &str, key: &str, value: &[u8]) -> Result<()> {
        check_no_nul("ns", ns)?;
        check_no_nul("key", key)?;
        self.inner
            .write()
            .unwrap()
            .insert((ns.to_string(), key.to_string()), value.to_vec());
        Ok(())
    }
    fn delete(&self, ns: &str, key: &str) -> Result<bool> {
        check_no_nul("ns", ns)?;
        check_no_nul("key", key)?;
        Ok(self
            .inner
            .write()
            .unwrap()
            .remove(&(ns.to_string(), key.to_string()))
            .is_some())
    }
    fn exists(&self, ns: &str, key: &str) -> Result<bool> {
        Ok(self.get(ns, key)?.is_some())
    }
    fn keys(&self, ns: &str) -> Result<Vec<String>> {
        check_no_nul("ns", ns)?;
        let g = self.inner.read().unwrap();
        Ok(g.keys()
            .filter(|(n, _)| n == ns)
            .map(|(_, k)| k.clone())
            .collect())
    }
    fn clear(&self, ns: &str) -> Result<()> {
        check_no_nul("ns", ns)?;
        self.inner.write().unwrap().retain(|(n, _), _| n != ns);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite(store: &dyn MemoryStore) {
        // roundtrip + absent
        assert_eq!(store.get("a", "k").unwrap(), None);
        store.set("a", "k", b"v1").unwrap();
        assert_eq!(store.get("a", "k").unwrap(), Some(b"v1".to_vec()));
        assert!(store.exists("a", "k").unwrap());
        // overwrite
        store.set("a", "k", b"v2").unwrap();
        assert_eq!(store.get("a", "k").unwrap(), Some(b"v2".to_vec()));
        // namespace isolation: same key, different ns
        store.set("b", "k", b"other").unwrap();
        assert_eq!(store.get("a", "k").unwrap(), Some(b"v2".to_vec()));
        assert_eq!(store.keys("a").unwrap(), vec!["k".to_string()]);
        // delete returns existed
        assert!(store.delete("a", "k").unwrap());
        assert!(!store.delete("a", "k").unwrap());
        assert!(!store.exists("a", "k").unwrap());
        // clear wipes only its ns
        store.set("a", "x", b"1").unwrap();
        store.set("a", "y", b"2").unwrap();
        store.clear("a").unwrap();
        assert!(store.keys("a").unwrap().is_empty());
        assert_eq!(store.get("b", "k").unwrap(), Some(b"other".to_vec()));
        // NUL rejected
        assert!(matches!(
            store.set("a\0", "k", b"v"),
            Err(MemoryError::InvalidKey(_))
        ));
    }

    #[test]
    fn mem_store_satisfies_contract() {
        suite(&MemMemoryStore::new());
    }
}

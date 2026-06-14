//! redb-backed `MemoryStore`. One table `kv`, composite key `<ns>\0<key>`.
//! Namespace scans use the half-open byte range `[<ns>\0, <ns>\1)`.
//!
//! `clear()` uses range-collect-then-remove (one write txn) rather than
//! `extract_from_if` — unambiguously correct and free of `extract_if` API
//! drift across redb 4.x point releases.

use std::path::Path;

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

use crate::{MemoryError, MemoryStore, Result, check_no_nul};

const KV: TableDefinition<&[u8], &[u8]> = TableDefinition::new("kv");

pub struct RedbStore {
    db: Database,
}

fn backend<E: std::fmt::Display>(e: E) -> MemoryError {
    MemoryError::Backend(e.to_string())
}

/// `<ns>\0<key>`
fn composite(ns: &str, key: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(ns.len() + 1 + key.len());
    v.extend_from_slice(ns.as_bytes());
    v.push(0);
    v.extend_from_slice(key.as_bytes());
    v
}

/// Inclusive lower bound `<ns>\0` and exclusive upper bound `<ns>\1` for a
/// half-open prefix scan over one namespace.
fn ns_bounds(ns: &str) -> (Vec<u8>, Vec<u8>) {
    let mut lo = ns.as_bytes().to_vec();
    lo.push(0);
    let mut hi = ns.as_bytes().to_vec();
    hi.push(1);
    (lo, hi)
}

impl RedbStore {
    /// Open or create the database file and ensure the `kv` table exists (so
    /// read transactions never hit `TableDoesNotExist` on a fresh file).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = Database::create(path).map_err(backend)?;
        let wtx = db.begin_write().map_err(backend)?;
        {
            wtx.open_table(KV).map_err(backend)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(Self { db })
    }
}

impl MemoryStore for RedbStore {
    fn get(&self, ns: &str, key: &str) -> Result<Option<Vec<u8>>> {
        check_no_nul("ns", ns)?;
        check_no_nul("key", key)?;
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(KV).map_err(backend)?;
        let ck = composite(ns, key);
        let got = table.get(ck.as_slice()).map_err(backend)?;
        Ok(got.map(|g| g.value().to_vec()))
    }

    fn set(&self, ns: &str, key: &str, value: &[u8]) -> Result<()> {
        check_no_nul("ns", ns)?;
        check_no_nul("key", key)?;
        let ck = composite(ns, key);
        let wtx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = wtx.open_table(KV).map_err(backend)?;
            table.insert(ck.as_slice(), value).map_err(backend)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(())
    }

    fn delete(&self, ns: &str, key: &str) -> Result<bool> {
        check_no_nul("ns", ns)?;
        check_no_nul("key", key)?;
        let ck = composite(ns, key);
        let wtx = self.db.begin_write().map_err(backend)?;
        let existed;
        {
            let mut table = wtx.open_table(KV).map_err(backend)?;
            existed = table.remove(ck.as_slice()).map_err(backend)?.is_some();
        }
        wtx.commit().map_err(backend)?;
        Ok(existed)
    }

    fn exists(&self, ns: &str, key: &str) -> Result<bool> {
        Ok(self.get(ns, key)?.is_some())
    }

    fn keys(&self, ns: &str) -> Result<Vec<String>> {
        check_no_nul("ns", ns)?;
        let (lo, hi) = ns_bounds(ns);
        let prefix_len = ns.len() + 1; // strip "<ns>\0"
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(KV).map_err(backend)?;
        let mut out = Vec::new();
        let iter = table.range(lo.as_slice()..hi.as_slice()).map_err(backend)?;
        for item in iter {
            let (k, _v) = item.map_err(backend)?;
            let bytes = k.value();
            let key = std::str::from_utf8(&bytes[prefix_len..])
                .map_err(backend)?
                .to_string();
            out.push(key);
        }
        Ok(out) // redb yields in sorted key order
    }

    fn clear(&self, ns: &str) -> Result<()> {
        check_no_nul("ns", ns)?;
        let (lo, hi) = ns_bounds(ns);
        let wtx = self.db.begin_write().map_err(backend)?;
        {
            let mut table = wtx.open_table(KV).map_err(backend)?;
            // Collect composite keys in range, then remove (range borrow ends
            // before the mutable removals). One txn → atomic.
            let mut doomed: Vec<Vec<u8>> = Vec::new();
            {
                let iter = table.range(lo.as_slice()..hi.as_slice()).map_err(backend)?;
                for item in iter {
                    let (k, _v) = item.map_err(backend)?;
                    doomed.push(k.value().to_vec());
                }
            }
            for k in doomed {
                table.remove(k.as_slice()).map_err(backend)?;
            }
        }
        wtx.commit().map_err(backend)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, RedbStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = RedbStore::open(dir.path().join("m.redb")).unwrap();
        (dir, store)
    }

    #[test]
    fn redb_roundtrip_and_isolation() {
        let (_d, s) = tmp();
        assert_eq!(s.get("a", "k").unwrap(), None);
        s.set("a", "k", b"v").unwrap();
        s.set("b", "k", b"other").unwrap();
        assert_eq!(s.get("a", "k").unwrap(), Some(b"v".to_vec()));
        assert_eq!(s.keys("a").unwrap(), vec!["k".to_string()]);
        assert!(s.delete("a", "k").unwrap());
        assert!(!s.delete("a", "k").unwrap());
        assert_eq!(s.get("b", "k").unwrap(), Some(b"other".to_vec()));
    }

    #[test]
    fn redb_clear_only_target_namespace() {
        let (_d, s) = tmp();
        s.set("a", "x", b"1").unwrap();
        s.set("a", "y", b"2").unwrap();
        s.set("b", "z", b"3").unwrap();
        s.clear("a").unwrap();
        assert!(s.keys("a").unwrap().is_empty());
        assert_eq!(s.keys("b").unwrap(), vec!["z".to_string()]);
    }

    #[test]
    fn redb_composite_key_no_collision() {
        // ns="a" key="b/c"  vs  ns="a/b" key="c": must not collide.
        let (_d, s) = tmp();
        s.set("a", "b/c", b"first").unwrap();
        s.set("a/b", "c", b"second").unwrap();
        assert_eq!(s.get("a", "b/c").unwrap(), Some(b"first".to_vec()));
        assert_eq!(s.get("a/b", "c").unwrap(), Some(b"second".to_vec()));
        assert_eq!(s.keys("a").unwrap(), vec!["b/c".to_string()]);
        assert_eq!(s.keys("a/b").unwrap(), vec!["c".to_string()]);
    }

    #[test]
    fn redb_high_byte_key_in_range() {
        // A key whose bytes include high values must still fall in the scan.
        let (_d, s) = tmp();
        let hi_key = "\u{00ff}"; // UTF-8 0xC3 0xBF
        s.set("a", hi_key, b"v").unwrap();
        s.set("a", "z", b"w").unwrap();
        let mut ks = s.keys("a").unwrap();
        ks.sort();
        assert!(ks.contains(&hi_key.to_string()));
        assert!(ks.contains(&"z".to_string()));
    }

    #[test]
    fn redb_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.redb");
        {
            let s = RedbStore::open(&path).unwrap();
            s.set("a", "k", b"durable").unwrap();
        }
        let s2 = RedbStore::open(&path).unwrap();
        assert_eq!(s2.get("a", "k").unwrap(), Some(b"durable".to_vec()));
    }

    #[test]
    fn redb_rejects_nul() {
        let (_d, s) = tmp();
        assert!(matches!(
            s.set("a\0b", "k", b"v"),
            Err(MemoryError::InvalidKey(_))
        ));
        assert!(matches!(
            s.set("a", "k\0", b"v"),
            Err(MemoryError::InvalidKey(_))
        ));
    }
}

//! Real OS keyring smoke test. Gated; requires:
//!   AGENTD_TEST_KEYRING=1
//! plus a working keyring backend on the host (libsecret/D-Bus, Keychain, ...).

use agentd_secrets::{KeyringStore, SecretStore};

fn gated() -> bool {
    std::env::var("AGENTD_TEST_KEYRING").ok().as_deref() == Some("1")
}

#[test]
fn keyring_roundtrip() {
    if !gated() {
        eprintln!("skip: set AGENTD_TEST_KEYRING=1 to enable");
        return;
    }
    let store = KeyringStore::new("agentd-test");
    let key = format!("rt-{}", std::process::id());
    let _ = store.delete(&key);
    store.set(&key, "hello").unwrap();
    assert_eq!(store.get(&key).unwrap(), "hello");
    store.delete(&key).unwrap();
    assert!(store.try_get(&key).unwrap().is_none());
}

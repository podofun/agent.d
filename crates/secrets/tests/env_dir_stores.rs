//! Tests for the container-friendly read-only backends: `EnvStore` and `DirStore`.

use agentd_secrets::{DirStore, EnvStore, SecretError, SecretStore};

// ---------- EnvStore ----------

#[test]
fn env_get_reads_prefixed_variable() {
    unsafe { std::env::set_var("AGENTD_SECRET_ENV_GET_TOKEN", "s3cret") };
    let s = EnvStore::new();
    assert_eq!(s.get("env_get_token").unwrap(), "s3cret");
    unsafe { std::env::remove_var("AGENTD_SECRET_ENV_GET_TOKEN") };
}

#[test]
fn env_get_maps_dashes_to_underscores() {
    unsafe { std::env::set_var("AGENTD_SECRET_ENV_DASH_KEY", "v") };
    let s = EnvStore::new();
    assert_eq!(s.get("env-dash-key").unwrap(), "v");
    unsafe { std::env::remove_var("AGENTD_SECRET_ENV_DASH_KEY") };
}

#[test]
fn env_get_missing_is_not_found() {
    let s = EnvStore::new();
    assert!(matches!(
        s.get("definitely_not_set_anywhere"),
        Err(SecretError::NotFound(_))
    ));
    assert!(s.try_get("definitely_not_set_anywhere").unwrap().is_none());
}

#[test]
fn env_list_returns_lowercased_keys_with_prefix_stripped() {
    unsafe { std::env::set_var("AGENTD_SECRET_ENV_LIST_ALPHA", "a") };
    let s = EnvStore::new();
    let keys = s.list().unwrap();
    assert!(keys.contains(&"env_list_alpha".to_string()), "got {keys:?}");
    unsafe { std::env::remove_var("AGENTD_SECRET_ENV_LIST_ALPHA") };
}

#[test]
fn env_is_read_only() {
    let s = EnvStore::new();
    assert!(matches!(s.set("k", "v"), Err(SecretError::Backend(_))));
    assert!(matches!(s.delete("k"), Err(SecretError::Backend(_))));
}

// ---------- DirStore ----------

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("agentd-secrets-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn dir_get_reads_file_contents() {
    let dir = temp_dir("get");
    std::fs::write(dir.join("api_key"), "hunter2").unwrap();
    let s = DirStore::new(&dir);
    assert_eq!(s.get("api_key").unwrap(), "hunter2");
}

#[test]
fn dir_get_trims_trailing_newline() {
    let dir = temp_dir("trim");
    std::fs::write(dir.join("api_key"), "hunter2\n").unwrap();
    let s = DirStore::new(&dir);
    assert_eq!(s.get("api_key").unwrap(), "hunter2");
}

#[test]
fn dir_get_missing_is_not_found() {
    let dir = temp_dir("missing");
    let s = DirStore::new(&dir);
    assert!(matches!(s.get("nope"), Err(SecretError::NotFound(_))));
}

#[test]
fn dir_rejects_path_traversal_keys() {
    let dir = temp_dir("traversal");
    let s = DirStore::new(&dir);
    assert!(matches!(
        s.get("../etc/passwd"),
        Err(SecretError::Backend(_))
    ));
    assert!(matches!(s.get("a/b"), Err(SecretError::Backend(_))));
    assert!(matches!(s.get(""), Err(SecretError::Backend(_))));
}

#[test]
fn dir_list_returns_file_names_sorted() {
    let dir = temp_dir("list");
    std::fs::write(dir.join("beta"), "2").unwrap();
    std::fs::write(dir.join("alpha"), "1").unwrap();
    let s = DirStore::new(&dir);
    assert_eq!(
        s.list().unwrap(),
        vec!["alpha".to_string(), "beta".to_string()]
    );
}

#[cfg(unix)]
#[test]
fn dir_list_follows_symlinks_like_kubernetes_mounts() {
    let dir = temp_dir("symlink");
    std::fs::create_dir(dir.join("..data")).unwrap();
    std::fs::write(dir.join("..data").join("api_key"), "v").unwrap();
    std::os::unix::fs::symlink(dir.join("..data").join("api_key"), dir.join("api_key")).unwrap();
    let s = DirStore::new(&dir);
    assert_eq!(s.list().unwrap(), vec!["api_key".to_string()]);
    assert_eq!(s.get("api_key").unwrap(), "v");
}

#[test]
fn dir_is_read_only() {
    let dir = temp_dir("readonly");
    let s = DirStore::new(&dir);
    assert!(matches!(s.set("k", "v"), Err(SecretError::Backend(_))));
    assert!(matches!(s.delete("k"), Err(SecretError::Backend(_))));
}

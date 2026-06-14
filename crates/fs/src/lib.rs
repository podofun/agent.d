//! Filesystem primitive.
//!
//! No permission enforcement here. The scripting/context layer gates by
//! `fs.read:<path>` / `fs.write:<path>` before calling these functions.
//! Paths SHOULD be absolute by the time they reach this crate so that the
//! permission slug the gate checks is unambiguous; callers may normalize
//! relative inputs first.

use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("not found: {0}")]
    NotFound(PathBuf),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid utf-8 in `{0}`")]
    InvalidUtf8(PathBuf),
}

#[derive(Debug, Clone, Serialize)]
pub struct Stat {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub readonly: bool,
    pub modified_unix: Option<i64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize)]
pub struct DirEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
}

pub async fn read_to_string(path: &Path) -> Result<String, FsError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FsError::NotFound(path.into())),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            Err(FsError::InvalidUtf8(path.into()))
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn read_bytes(path: &Path) -> Result<Vec<u8>, FsError> {
    match tokio::fs::read(path).await {
        Ok(v) => Ok(v),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FsError::NotFound(path.into())),
        Err(e) => Err(e.into()),
    }
}

pub async fn write(path: &Path, contents: &[u8]) -> Result<(), FsError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, contents).await?;
    Ok(())
}

pub async fn append(path: &Path, contents: &[u8]) -> Result<(), FsError> {
    use tokio::io::AsyncWriteExt;
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(contents).await?;
    f.flush().await?;
    Ok(())
}

pub async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

pub async fn stat(path: &Path) -> Result<Stat, FsError> {
    let meta = match tokio::fs::metadata(path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(FsError::NotFound(path.into()));
        }
        Err(e) => return Err(e.into()),
    };
    let kind = if meta.is_file() {
        EntryKind::File
    } else if meta.is_dir() {
        EntryKind::Dir
    } else if meta.file_type().is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    let modified_unix = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    Ok(Stat {
        path: path.to_path_buf(),
        kind,
        size: meta.len(),
        readonly: meta.permissions().readonly(),
        modified_unix,
    })
}

pub async fn list_dir(path: &Path) -> Result<Vec<DirEntry>, FsError> {
    let mut out = Vec::new();
    let mut rd = match tokio::fs::read_dir(path).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(FsError::NotFound(path.into()));
        }
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = rd.next_entry().await? {
        let ft = entry.file_type().await?;
        let kind = if ft.is_file() {
            EntryKind::File
        } else if ft.is_dir() {
            EntryKind::Dir
        } else if ft.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        out.push(DirEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            path: entry.path(),
            kind,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub async fn remove_file(path: &Path) -> Result<(), FsError> {
    tokio::fs::remove_file(path).await.map_err(Into::into)
}

pub async fn remove_dir_all(path: &Path) -> Result<(), FsError> {
    tokio::fs::remove_dir_all(path).await.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[tokio::test]
    async fn write_and_read_string() {
        let dir = tmpdir().await;
        let p = dir.path().join("a.txt");
        write(&p, b"hello").await.unwrap();
        let s = read_to_string(&p).await.unwrap();
        assert_eq!(s, "hello");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = tmpdir().await;
        let p = dir.path().join("deep/nested/file.txt");
        write(&p, b"x").await.unwrap();
        assert!(exists(&p).await);
    }

    #[tokio::test]
    async fn read_missing_is_notfound() {
        let dir = tmpdir().await;
        let err = read_to_string(&dir.path().join("missing"))
            .await
            .unwrap_err();
        assert!(matches!(err, FsError::NotFound(_)));
    }

    #[tokio::test]
    async fn append_extends_file() {
        let dir = tmpdir().await;
        let p = dir.path().join("log.txt");
        append(&p, b"a\n").await.unwrap();
        append(&p, b"b\n").await.unwrap();
        let s = read_to_string(&p).await.unwrap();
        assert_eq!(s, "a\nb\n");
    }

    #[tokio::test]
    async fn stat_reports_file_kind() {
        let dir = tmpdir().await;
        let p = dir.path().join("a.txt");
        write(&p, b"hi").await.unwrap();
        let st = stat(&p).await.unwrap();
        assert_eq!(st.kind, EntryKind::File);
        assert_eq!(st.size, 2);
    }

    #[tokio::test]
    async fn list_dir_sorted() {
        let dir = tmpdir().await;
        write(&dir.path().join("b.txt"), b"").await.unwrap();
        write(&dir.path().join("a.txt"), b"").await.unwrap();
        tokio::fs::create_dir(&dir.path().join("c")).await.unwrap();
        let entries = list_dir(dir.path()).await.unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c"]);
    }

    #[tokio::test]
    async fn remove_file_works() {
        let dir = tmpdir().await;
        let p = dir.path().join("a.txt");
        write(&p, b"").await.unwrap();
        remove_file(&p).await.unwrap();
        assert!(!exists(&p).await);
    }
}

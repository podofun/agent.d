//! Binary file history. Callers own authorization and absolute path resolution.

use std::collections::HashMap;
use std::fs::{self, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::FsError;

/// Shared in-memory history; clone this handle across runtime reloads.
/// Operations on the same path serialize, including their filesystem commit.
#[derive(Clone, Default)]
pub struct History {
    files: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<Timeline>>>>>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Write,
    Append,
    Remove,
}

#[derive(Debug, Clone, Serialize)]
pub struct Revision {
    pub id: usize,
    pub parent: usize,
    pub operation: Operation,
    pub timestamp_ms: u64,
    pub offset: usize,
    pub removed_bytes: usize,
    pub added_bytes: usize,
    pub before_size: Option<usize>,
    pub after_size: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileHistory {
    pub current: usize,
    pub revisions: Vec<Revision>,
}

/// One reversible byte splice. Offsets and lengths are bytes, never characters.
#[derive(Debug, Clone)]
pub struct FileDiff {
    pub revision: Revision,
    pub removed: Vec<u8>,
    pub added: Vec<u8>,
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    digest: [u8; 32],
    permissions: Permissions,
}

struct Image {
    bytes: Option<Vec<u8>>,
    permissions: Option<Permissions>,
}

impl Image {
    fn read(path: &Path) -> Result<Self, FsError> {
        validate_ancestors(path)?;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    bytes: None,
                    permissions: None,
                });
            }
            Err(e) => return Err(e.into()),
        };
        if !metadata.is_file() {
            return Err(FsError::Unsupported(path.into()));
        }
        // Atomic replacement cannot preserve hard-link identity.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(FsError::Unsupported(path.into()));
            }
        }
        Ok(Self {
            bytes: Some(fs::read(path)?),
            permissions: Some(metadata.permissions()),
        })
    }

    fn stamp(&self) -> Option<Stamp> {
        self.bytes.as_ref().map(|bytes| Stamp {
            digest: Sha256::digest(bytes).into(),
            permissions: self.permissions.clone().expect("file permissions"),
        })
    }
}

struct Change {
    diff: FileDiff,
    before: Option<Stamp>,
    after: Option<Stamp>,
    created_dirs: Vec<PathBuf>,
}

#[derive(Default)]
struct Timeline {
    changes: Vec<Change>,
    current: usize,
    redo: Vec<usize>,
}

impl Timeline {
    fn expected(&self) -> Option<&Option<Stamp>> {
        if self.current == 0 {
            self.changes.first().map(|c| &c.before)
        } else {
            Some(&self.changes[self.current - 1].after)
        }
    }

    fn checked_image(&self, path: &Path) -> Result<(Image, Option<Stamp>), FsError> {
        let image = Image::read(path)?;
        let stamp = image.stamp();
        if let Some(expected) = self.expected()
            && &stamp != expected
        {
            return Err(FsError::Conflict(path.into()));
        }
        Ok((image, stamp))
    }

    fn mutate(
        &mut self,
        path: &Path,
        operation: Operation,
        content: Vec<u8>,
    ) -> Result<usize, FsError> {
        let (before, before_stamp) = self.checked_image(path)?;
        let bytes = match operation {
            Operation::Write => Some(content),
            Operation::Append => {
                let mut bytes = before.bytes.clone().unwrap_or_default();
                bytes.extend_from_slice(&content);
                Some(bytes)
            }
            Operation::Remove => {
                if before.bytes.is_none() {
                    return Err(FsError::NotFound(path.into()));
                }
                None
            }
        };
        let old = before.bytes.as_deref().unwrap_or_default();
        let new = bytes.as_deref().unwrap_or_default();
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let removed = old[prefix..old.len() - suffix].to_vec();
        let added = new[prefix..new.len() - suffix].to_vec();
        let revision = Revision {
            id: self.changes.len() + 1,
            parent: self.current,
            operation,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            offset: prefix,
            removed_bytes: removed.len(),
            added_bytes: added.len(),
            before_size: before.bytes.as_ref().map(Vec::len),
            after_size: bytes.as_ref().map(Vec::len),
        };
        let mut after = Image {
            bytes,
            permissions: before.permissions.clone(),
        };
        let created_dirs = commit(path, &mut after)?;
        let id = revision.id;
        self.changes.push(Change {
            diff: FileDiff {
                revision,
                removed,
                added,
            },
            before: before_stamp,
            after: after.stamp(),
            created_dirs,
        });
        self.current = id;
        self.redo.clear();
        Ok(id)
    }

    fn parent(&self, id: usize) -> usize {
        self.changes[id - 1].diff.revision.parent
    }

    fn restore(&mut self, path: &Path, target: usize) -> Result<usize, FsError> {
        if target > self.changes.len() {
            return Err(FsError::UnknownRevision(target));
        }
        let (mut image, _) = self.checked_image(path)?;
        let (mut left, mut right) = (self.current, target);
        let mut backward = Vec::new();
        let mut forward = Vec::new();
        // IDs increase along every branch, so the larger ID can walk first.
        while left != right {
            if left > right {
                backward.push(left);
                left = self.parent(left);
            } else {
                forward.push(right);
                right = self.parent(right);
            }
        }
        for &id in &backward {
            apply(&mut image, &self.changes[id - 1], false);
        }
        for &id in forward.iter().rev() {
            apply(&mut image, &self.changes[id - 1], true);
        }
        if self.current != target {
            commit(path, &mut image)?;
            // Only remove directories this history created, and only if empty.
            for id in backward {
                for dir in &self.changes[id - 1].created_dirs {
                    let _ = fs::remove_dir(dir);
                }
            }
            self.current = target;
        }
        Ok(target)
    }
}

fn apply(image: &mut Image, change: &Change, forward: bool) {
    let diff = &change.diff;
    let (remove, insert, stamp) = if forward {
        (&diff.removed, &diff.added, &change.after)
    } else {
        (&diff.added, &diff.removed, &change.before)
    };
    if let Some(stamp) = stamp {
        let bytes = image.bytes.get_or_insert_with(Vec::new);
        bytes.splice(
            diff.revision.offset..diff.revision.offset + remove.len(),
            insert.iter().copied(),
        );
        image.permissions = Some(stamp.permissions.clone());
    } else {
        image.bytes = None;
        image.permissions = None;
    }
}

/// Reject ancestors replaced with symlinks after permission resolution.
fn validate_ancestors(path: &Path) -> Result<(), FsError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(FsError::Unsupported(path.into()));
    }
    for parent in path.ancestors().skip(1) {
        match fs::symlink_metadata(parent) {
            Ok(meta) if !meta.is_dir() => return Err(FsError::Unsupported(parent.into())),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Stage writes alongside the target, then atomically replace it. A failed
/// write never truncates the previous file or advances the history cursor.
fn commit(path: &Path, image: &mut Image) -> Result<Vec<PathBuf>, FsError> {
    let Some(bytes) = &image.bytes else {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        return Ok(Vec::new());
    };
    let parent = path
        .parent()
        .ok_or_else(|| FsError::Unsupported(path.into()))?;
    let mut created_dirs = Vec::new();
    for dir in parent.ancestors() {
        match fs::symlink_metadata(dir) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                created_dirs.push(dir.to_path_buf())
            }
            Err(e) => return Err(e.into()),
        }
    }
    let result = (|| -> Result<(), FsError> {
        fs::create_dir_all(parent)?;
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        if image.permissions.is_none() {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(Permissions::from_mode(0o666));
        }
        let mut file = builder.tempfile_in(parent)?;
        file.write_all(bytes)?;
        if let Some(permissions) = &image.permissions {
            file.as_file().set_permissions(permissions.clone())?;
        }
        image.permissions = Some(file.as_file().metadata()?.permissions());
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    })();
    if result.is_err() {
        for dir in &created_dirs {
            let _ = fs::remove_dir(dir);
        }
    }
    result?;
    Ok(created_dirs)
}

impl History {
    async fn with_file<T: Send + 'static>(
        &self,
        path: &Path,
        f: impl FnOnce(&mut Timeline, &Path) -> Result<T, FsError> + Send + 'static,
    ) -> Result<T, FsError> {
        let path = path.to_path_buf();
        let files = self.files.clone();
        // Once started, mutation and journal publication finish together even
        // when the calling action is cancelled.
        tokio::task::spawn_blocking(move || {
            let timeline = files
                .lock()
                .map_err(|_| FsError::HistoryPoisoned)?
                .entry(path.clone())
                .or_default()
                .clone();
            let mut timeline = timeline.lock().map_err(|_| FsError::HistoryPoisoned)?;
            f(&mut timeline, &path)
        })
        .await
        .map_err(|e| FsError::Io(std::io::Error::other(e)))?
    }

    pub async fn write(&self, path: &Path, bytes: &[u8]) -> Result<usize, FsError> {
        let bytes = bytes.to_vec();
        self.with_file(path, move |t, p| t.mutate(p, Operation::Write, bytes))
            .await
    }

    pub async fn append(&self, path: &Path, bytes: &[u8]) -> Result<usize, FsError> {
        let bytes = bytes.to_vec();
        self.with_file(path, move |t, p| t.mutate(p, Operation::Append, bytes))
            .await
    }

    pub async fn remove(&self, path: &Path) -> Result<usize, FsError> {
        self.with_file(path, |t, p| t.mutate(p, Operation::Remove, Vec::new()))
            .await
    }

    pub async fn history(&self, path: &Path) -> Result<FileHistory, FsError> {
        self.with_file(path, |t, _| {
            Ok(FileHistory {
                current: t.current,
                revisions: t.changes.iter().map(|c| c.diff.revision.clone()).collect(),
            })
        })
        .await
    }

    pub async fn diff(&self, path: &Path, revision: usize) -> Result<FileDiff, FsError> {
        self.with_file(path, move |t, _| {
            revision
                .checked_sub(1)
                .and_then(|index| t.changes.get(index))
                .map(|c| c.diff.clone())
                .ok_or(FsError::UnknownRevision(revision))
        })
        .await
    }

    pub async fn restore(&self, path: &Path, revision: usize) -> Result<usize, FsError> {
        self.with_file(path, move |t, p| {
            let result = t.restore(p, revision)?;
            t.redo.clear();
            Ok(result)
        })
        .await
    }

    pub async fn undo(&self, path: &Path) -> Result<usize, FsError> {
        self.with_file(path, |t, p| {
            let current = t.current;
            if current == 0 {
                return Err(FsError::NoUndo);
            }
            let result = t.restore(p, t.parent(current))?;
            t.redo.push(current);
            Ok(result)
        })
        .await
    }

    pub async fn redo(&self, path: &Path) -> Result<usize, FsError> {
        self.with_file(path, |t, p| {
            let target = *t.redo.last().ok_or(FsError::NoRedo)?;
            let result = t.restore(p, target)?;
            t.redo.pop();
            Ok(result)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("data.bin");
        (dir, path)
    }

    #[tokio::test]
    async fn binary_create_append_delete_undo_redo() {
        let (_dir, path) = temp();
        let history = History::default();
        assert_eq!(history.write(&path, &[0, 255, 128]).await.unwrap(), 1);
        assert_eq!(history.append(&path, &[254, 0]).await.unwrap(), 2);
        assert_eq!(history.remove(&path).await.unwrap(), 3);
        assert!(!path.exists());
        assert_eq!(history.undo(&path).await.unwrap(), 2);
        assert_eq!(fs::read(&path).unwrap(), [0, 255, 128, 254, 0]);
        assert_eq!(history.undo(&path).await.unwrap(), 1);
        assert_eq!(fs::read(&path).unwrap(), [0, 255, 128]);
        assert_eq!(history.undo(&path).await.unwrap(), 0);
        assert!(!path.exists());
        assert!(matches!(history.undo(&path).await, Err(FsError::NoUndo)));
        assert_eq!(history.redo(&path).await.unwrap(), 1);
        assert_eq!(history.redo(&path).await.unwrap(), 2);
        assert_eq!(history.redo(&path).await.unwrap(), 3);
        assert!(!path.exists());
        assert!(matches!(history.redo(&path).await, Err(FsError::NoRedo)));
    }

    #[tokio::test]
    async fn original_file_and_abandoned_branches_remain_restorable() {
        let (_dir, path) = temp();
        fs::write(&path, b"original").unwrap();
        let history = History::default();
        history.write(&path, b"first").await.unwrap();
        history.write(&path, b"second").await.unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(history.write(&path, b"branch").await.unwrap(), 3);
        assert!(matches!(history.redo(&path).await, Err(FsError::NoRedo)));
        for (revision, bytes) in [(2, "second"), (3, "branch"), (0, "original"), (1, "first")] {
            history.restore(&path, revision).await.unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes.as_bytes());
        }
        let log = history.history(&path).await.unwrap();
        assert_eq!(log.current, 1);
        assert_eq!(log.revisions.len(), 3);
        assert_eq!(log.revisions[2].parent, 1);
    }

    #[tokio::test]
    async fn empty_file_is_distinct_from_missing_and_noops_are_recorded() {
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"").await.unwrap();
        history.write(&path, b"").await.unwrap();
        assert_eq!(history.history(&path).await.unwrap().revisions.len(), 2);
        history.restore(&path, 0).await.unwrap();
        assert!(!path.exists());
        history.restore(&path, 2).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
    }

    #[tokio::test]
    async fn small_binary_edit_retains_only_changed_bytes() {
        let (_dir, path) = temp();
        let mut bytes = vec![128; 4 * 1024 * 1024];
        fs::write(&path, &bytes).unwrap();
        let history = History::default();
        bytes[2 * 1024 * 1024] = 255;
        history.write(&path, &bytes).await.unwrap();
        let diff = history.diff(&path, 1).await.unwrap();
        assert_eq!(diff.revision.offset, 2 * 1024 * 1024);
        assert_eq!(diff.removed, [128]);
        assert_eq!(diff.added, [255]);
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap()[2 * 1024 * 1024], 128);
    }

    #[tokio::test]
    async fn conflicts_do_not_overwrite_disk_or_advance_history() {
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"tracked").await.unwrap();
        fs::write(&path, b"outside edit").unwrap();
        assert!(matches!(
            history.undo(&path).await,
            Err(FsError::Conflict(_))
        ));
        assert!(matches!(
            history.write(&path, b"lost").await,
            Err(FsError::Conflict(_))
        ));
        assert!(matches!(
            history.append(&path, b"lost").await,
            Err(FsError::Conflict(_))
        ));
        assert!(matches!(
            history.remove(&path).await,
            Err(FsError::Conflict(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"outside edit");
        let log = history.history(&path).await.unwrap();
        assert_eq!(log.current, 1);
        assert_eq!(log.revisions.len(), 1);
        fs::write(&path, b"tracked").unwrap();
        history.undo(&path).await.unwrap();
        fs::write(&path, b"new outside file").unwrap();
        assert!(matches!(
            history.redo(&path).await,
            Err(FsError::Conflict(_))
        ));
        fs::remove_file(&path).unwrap();
        assert_eq!(history.redo(&path).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn invalid_operations_leave_history_unchanged() {
        let (_dir, path) = temp();
        let history = History::default();
        assert!(history.remove(&path).await.is_err());
        assert!(history.restore(&path, 1).await.is_err());
        assert!(history.diff(&path, 0).await.is_err());
        fs::create_dir(&path).unwrap();
        assert!(history.write(&path, b"x").await.is_err());
        let log = history.history(&path).await.unwrap();
        assert_eq!(log.current, 0);
        assert!(log.revisions.is_empty());
    }

    #[tokio::test]
    async fn generated_binary_transitions_restore_in_any_order() {
        let (_dir, path) = temp();
        let history = History::default();
        let mut versions = Vec::new();
        let mut seed = 17_u32;
        for n in 0..40 {
            let bytes: Vec<u8> = (0..n * 13)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 24) as u8
                })
                .collect();
            history.write(&path, &bytes).await.unwrap();
            versions.push(bytes);
        }
        for n in (0..40).step_by(3).chain((0..40).rev().step_by(2)) {
            history.restore(&path, n + 1).await.unwrap();
            assert_eq!(fs::read(&path).unwrap(), versions[n]);
        }
    }

    #[tokio::test]
    async fn concurrent_appends_serialize_without_lost_bytes() {
        let (_dir, path) = temp();
        let history = History::default();
        let mut tasks = Vec::new();
        for byte in 0..24 {
            let history = history.clone();
            let path = path.clone();
            tasks.push(tokio::spawn(async move {
                history.append(&path, &[byte]).await.unwrap()
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let mut bytes = fs::read(&path).unwrap();
        bytes.sort();
        assert_eq!(bytes, (0..24).collect::<Vec<_>>());
        assert_eq!(history.history(&path).await.unwrap().revisions.len(), 24);
        history.restore(&path, 0).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn undo_cleans_up_only_empty_created_parents() {
        let (dir, _) = temp();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("new/nested/data.bin");
        let history = History::default();
        history.write(&path, b"x").await.unwrap();
        history.undo(&path).await.unwrap();
        assert!(!root.join("new").exists());
        history.redo(&path).await.unwrap();
        fs::write(root.join("new/nested/untracked"), b"keep").unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(
            fs::read(root.join("new/nested/untracked")).unwrap(),
            b"keep"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mode_is_preserved_and_links_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, path) = temp();
        fs::write(&path, b"original").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        let history = History::default();
        history.write(&path, b"new").await.unwrap();
        history.remove(&path).await.unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"original");
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        assert!(matches!(
            history.write(&link, b"bad").await,
            Err(FsError::Unsupported(_))
        ));
        fs::remove_file(&link).unwrap();
        fs::hard_link(&path, &link).unwrap();
        assert!(matches!(
            history.write(&path, b"bad").await,
            Err(FsError::Unsupported(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }
}

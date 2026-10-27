//! A per-folder snapshot of the files in scope, so a caller can tell which
//! files changed between two scans however they were changed. Walking honors
//! `.gitignore`, `.ignore` and global git excludes without needing git, and
//! always skips `.git`, `target` and `node_modules`. A file whose size and
//! modification time are unchanged is not read again.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::FsError;

/// Content is kept for files of at most this many bytes.
pub const MAX_CONTENT_BYTES: u64 = 1024 * 1024;
/// A folder with more files than this is not snapshotted.
pub const MAX_FILES: usize = 50_000;

/// How much a scan keeps and how far it goes.
#[derive(Clone, Copy, Debug)]
pub struct ScanLimits {
    pub max_content_bytes: u64,
    pub max_files: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_content_bytes: MAX_CONTENT_BYTES,
            max_files: MAX_FILES,
        }
    }
}

/// One file as a scan saw it. `content` is `None` when the file is too large
/// or binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub size: u64,
    pub digest: [u8; 32],
    pub content: Option<Vec<u8>>,
}

/// A file that differs between two scans. `None` means absent.
#[derive(Clone, Debug)]
pub struct ScanChange {
    pub path: PathBuf,
    pub before: Option<Snapshot>,
    pub after: Option<Snapshot>,
}

/// Shared snapshots of every folder scanned so far. Clone this handle.
#[derive(Clone, Default)]
pub struct Baseline {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    roots: Mutex<HashMap<PathBuf, Arc<Files>>>,
    #[cfg(test)]
    hashes: std::sync::atomic::AtomicUsize,
}

type Files = HashMap<PathBuf, Entry>;

/// Folders a scan never enters, whatever the ignore files say.
const ALWAYS_IGNORED: [&str; 3] = [".git", "target", "node_modules"];
/// A file is binary when its first this many bytes hold a NUL.
const BINARY_PROBE: usize = 8 * 1024;
/// A file written this close to a scan may change again within the same
/// timestamp tick on a coarse clock, so the next scan reads it again.
const RACY_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct Entry {
    size: u64,
    modified: Option<SystemTime>,
    digest: [u8; 32],
    content: Option<Arc<[u8]>>,
    racy: bool,
}

impl Entry {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            size: self.size,
            digest: self.digest,
            content: self.content.as_deref().map(<[u8]>::to_vec),
        }
    }
}

impl Baseline {
    /// Scan `root`, keep what it holds, and return what changed since the
    /// last scan of the same root. The first scan of a root returns nothing.
    pub fn refresh(&self, root: &Path, limits: ScanLimits) -> Result<Vec<ScanChange>, FsError> {
        let previous = self
            .inner
            .roots
            .lock()
            .map_err(|_| FsError::HistoryPoisoned)?
            .get(root)
            .cloned();
        let started = SystemTime::now();
        let mut files = Files::new();
        for path in walk(root, limits.max_files)? {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let (size, modified) = (metadata.len(), metadata.modified().ok());
            let known = previous.as_ref().and_then(|p| p.get(&path));
            let entry = match known {
                Some(entry) if !entry.racy && entry.size == size && entry.modified == modified => {
                    entry.clone()
                }
                _ => match self.read(&path, size, modified, started, limits) {
                    Ok(entry) => entry,
                    // Gone or unreadable since the walk listed it.
                    Err(_) => continue,
                },
            };
            files.insert(path, entry);
        }
        let changes = match &previous {
            Some(previous) => compare(previous, &files),
            None => Vec::new(),
        };
        self.inner
            .roots
            .lock()
            .map_err(|_| FsError::HistoryPoisoned)?
            .insert(root.to_path_buf(), Arc::new(files));
        Ok(changes)
    }

    fn read(
        &self,
        path: &Path,
        size: u64,
        modified: Option<SystemTime>,
        started: SystemTime,
        limits: ScanLimits,
    ) -> std::io::Result<Entry> {
        #[cfg(test)]
        self.inner
            .hashes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let racy = modified.is_none_or(|m| m + RACY_WINDOW >= started);
        let mut file = std::fs::File::open(path)?;
        if size <= limits.max_content_bytes {
            let mut bytes = Vec::with_capacity(size as usize);
            file.read_to_end(&mut bytes)?;
            let digest = Sha256::digest(&bytes).into();
            let binary = bytes[..bytes.len().min(BINARY_PROBE)].contains(&0);
            return Ok(Entry {
                size: bytes.len() as u64,
                modified,
                digest,
                content: (!binary).then(|| Arc::from(bytes)),
                racy,
            });
        }
        let mut hasher = Sha256::new();
        let size = std::io::copy(&mut file, &mut hasher)?;
        Ok(Entry {
            size,
            modified,
            digest: hasher.finalize().into(),
            content: None,
            racy,
        })
    }
}

/// Every file under `root` that the ignore rules leave in, or an error when
/// there are more than `max_files`.
fn walk(root: &Path, max_files: usize) -> Result<Vec<PathBuf>, FsError> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !ALWAYS_IGNORED
                    .iter()
                    .any(|name| entry.file_name() == std::ffi::OsStr::new(name))
        })
        .build();
    let mut paths = Vec::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if paths.len() == max_files {
            return Err(FsError::TooManyFiles {
                root: root.to_path_buf(),
                limit: max_files,
            });
        }
        paths.push(entry.into_path());
    }
    Ok(paths)
}

fn compare(previous: &Files, current: &Files) -> Vec<ScanChange> {
    let mut changes = Vec::new();
    for (path, before) in previous {
        match current.get(path) {
            Some(after) if after.digest == before.digest => {}
            after => changes.push(ScanChange {
                path: path.clone(),
                before: Some(before.snapshot()),
                after: after.map(Entry::snapshot),
            }),
        }
    }
    for (path, after) in current {
        if !previous.contains_key(path) {
            changes.push(ScanChange {
                path: path.clone(),
                before: None,
                after: Some(after.snapshot()),
            });
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn folder() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        (dir, root)
    }

    fn content(snapshot: &Option<Snapshot>) -> Option<&[u8]> {
        snapshot.as_ref().and_then(|s| s.content.as_deref())
    }

    /// Move a file's modification time into the past, so a scan does not
    /// treat it as recently written.
    fn age(path: &Path) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
    }

    #[test]
    fn first_scan_reports_nothing() {
        let (_dir, root) = folder();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let baseline = Baseline::default();
        assert!(
            baseline
                .refresh(&root, ScanLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn changes_are_reported_once() {
        let (_dir, root) = folder();
        std::fs::write(root.join("a.txt"), "old").unwrap();
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        std::fs::write(root.join("a.txt"), "new").unwrap();
        let changes = baseline.refresh(&root, ScanLimits::default()).unwrap();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].path, root.join("a.txt"));
        assert_eq!(content(&changes[0].before), Some(&b"old"[..]));
        assert_eq!(content(&changes[0].after), Some(&b"new"[..]));
        assert!(
            baseline
                .refresh(&root, ScanLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn creations_and_deletions() {
        let (_dir, root) = folder();
        std::fs::write(root.join("gone.txt"), "x").unwrap();
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/new.txt"), "n").unwrap();
        let changes = baseline.refresh(&root, ScanLimits::default()).unwrap();
        assert_eq!(changes.len(), 2, "{changes:?}");
        let gone = changes
            .iter()
            .find(|c| c.path == root.join("gone.txt"))
            .unwrap();
        assert_eq!(content(&gone.before), Some(&b"x"[..]));
        assert!(gone.after.is_none());
        let new = changes
            .iter()
            .find(|c| c.path == root.join("sub/new.txt"))
            .unwrap();
        assert!(new.before.is_none());
        assert_eq!(content(&new.after), Some(&b"n"[..]));
    }

    #[test]
    fn ignored_files_are_skipped() {
        let (_dir, root) = folder();
        std::fs::write(root.join(".gitignore"), "out/\n").unwrap();
        for dir in ["out", ".git", "target", "node_modules"] {
            std::fs::create_dir(root.join(dir)).unwrap();
        }
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        std::fs::write(root.join("out/x"), "x").unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref").unwrap();
        std::fs::write(root.join("target/x"), "x").unwrap();
        std::fs::write(root.join("node_modules/x"), "x").unwrap();
        std::fs::write(root.join(".hidden"), "h").unwrap();
        let changes = baseline.refresh(&root, ScanLimits::default()).unwrap();
        let paths: Vec<_> = changes.iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, vec![root.join(".hidden")]);
    }

    #[test]
    fn large_and_binary_files_have_no_content() {
        let (_dir, root) = folder();
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        std::fs::write(root.join("big"), vec![b'a'; 2 * 1024 * 1024]).unwrap();
        std::fs::write(root.join("bin"), b"ab\0cd").unwrap();
        let changes = baseline.refresh(&root, ScanLimits::default()).unwrap();
        let big = changes.iter().find(|c| c.path == root.join("big")).unwrap();
        let after = big.after.as_ref().unwrap();
        assert_eq!(after.size, 2 * 1024 * 1024);
        assert!(after.content.is_none());
        let bin = changes.iter().find(|c| c.path == root.join("bin")).unwrap();
        let after = bin.after.as_ref().unwrap();
        assert_eq!(after.size, 5);
        assert!(after.content.is_none());
    }

    #[test]
    fn too_many_files_is_an_error() {
        let (_dir, root) = folder();
        for i in 0..5 {
            std::fs::write(root.join(format!("{i}.txt")), "x").unwrap();
        }
        let limits = ScanLimits {
            max_files: 3,
            ..Default::default()
        };
        let error = Baseline::default().refresh(&root, limits).unwrap_err();
        assert!(
            matches!(error, FsError::TooManyFiles { limit: 3, .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("more than 3 files"), "{error}");
    }

    #[test]
    fn unchanged_files_are_not_rehashed() {
        let (_dir, root) = folder();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.join(name), name).unwrap();
            age(&root.join(name));
        }
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        assert_eq!(baseline.inner.hashes.load(Ordering::SeqCst), 3);
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        assert_eq!(baseline.inner.hashes.load(Ordering::SeqCst), 3);
        std::fs::write(root.join("a.txt"), "changed").unwrap();
        let changes = baseline.refresh(&root, ScanLimits::default()).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(baseline.inner.hashes.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn a_same_size_rewrite_soon_after_a_scan_is_seen() {
        let (_dir, root) = folder();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        // Same size, and possibly the same timestamp on a coarse clock.
        std::fs::write(root.join("a.txt"), "b").unwrap();
        let changes = baseline.refresh(&root, ScanLimits::default()).unwrap();
        assert_eq!(changes.len(), 1, "{changes:?}");
    }
}

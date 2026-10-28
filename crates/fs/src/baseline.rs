//! A per-folder snapshot of the files in scope, so a caller can tell which
//! files changed between two scans however they were changed. Walking honors
//! `.gitignore`, `.ignore` and global git excludes without needing git, and
//! always skips `.git`, `target` and `node_modules`. A file whose size and
//! modification time are unchanged is not read again.
//!
//! File content is kept once per hash in a store shared by every folder, up
//! to a total size. Past it only hashes are kept. A folder's snapshot is
//! dropped after it goes unused for a while.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

use crate::FsError;

/// Content is kept for files of at most this many bytes.
pub const MAX_CONTENT_BYTES: u64 = 1024 * 1024;
/// A folder with more files than this is not snapshotted.
pub const MAX_FILES: usize = 50_000;
/// Content kept across all folders stops growing at this many bytes.
pub const MAX_STORED_BYTES: u64 = 256 * 1024 * 1024;
/// A folder found too large is not walked again for this long.
const TOO_LARGE_FOR: Duration = Duration::from_secs(5 * 60);
/// A folder's snapshot is dropped after going this long without a scan.
const IDLE_FOR: Duration = Duration::from_secs(30 * 60);

/// How much a scan keeps and how far it goes.
#[derive(Clone, Copy, Debug)]
pub struct ScanLimits {
    pub max_content_bytes: u64,
    pub max_files: usize,
    pub max_stored_bytes: u64,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_content_bytes: MAX_CONTENT_BYTES,
            max_files: MAX_FILES,
            max_stored_bytes: MAX_STORED_BYTES,
        }
    }
}

/// One file as a scan saw it. `content` is `None` when the file is too large
/// or binary, or when the content store was full.
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
    roots: Mutex<HashMap<PathBuf, Folder>>,
    /// Folders found too large, and when.
    too_large: Mutex<HashMap<PathBuf, Instant>>,
    store: Mutex<Store>,
    /// Folders locked by a call that is capturing its changes.
    held: Mutex<Vec<PathBuf>>,
    released: tokio::sync::Notify,
    scans: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    hashes: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    walks: std::sync::atomic::AtomicUsize,
}

struct Folder {
    files: Arc<Files>,
    used: Instant,
}

type Files = HashMap<PathBuf, Entry>;

/// File content by hash, shared by every folder.
#[derive(Default)]
struct Store {
    blobs: HashMap<[u8; 32], Arc<[u8]>>,
    bytes: u64,
}

impl Store {
    /// The stored copy of `bytes`, or `None` when the store is full.
    fn keep(&mut self, digest: [u8; 32], bytes: Vec<u8>, cap: u64) -> Option<Arc<[u8]>> {
        if let Some(blob) = self.blobs.get(&digest) {
            return Some(blob.clone());
        }
        let size = bytes.len() as u64;
        if self.bytes + size > cap {
            self.purge();
            if self.bytes + size > cap {
                return None;
            }
        }
        let blob: Arc<[u8]> = Arc::from(bytes);
        self.blobs.insert(digest, blob.clone());
        self.bytes += size;
        Some(blob)
    }

    /// Drop content no snapshot uses any more.
    fn purge(&mut self) {
        self.blobs.retain(|_, blob| Arc::strong_count(blob) > 1);
        self.bytes = self.blobs.values().map(|b| b.len() as u64).sum();
    }
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

/// Holds folders for one call until dropped. See [`Baseline::lock`].
pub struct FolderLock {
    inner: Arc<Inner>,
    roots: Vec<PathBuf>,
}

impl FolderLock {
    /// The folders this lock holds.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }
}

impl Drop for FolderLock {
    fn drop(&mut self) {
        {
            let mut held = locked(&self.inner.held);
            for root in &self.roots {
                if let Some(at) = held.iter().position(|h| h == root) {
                    held.swap_remove(at);
                }
            }
        }
        self.inner.released.notify_waiters();
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Whether a file can be read safely as itself: a regular file with one link.
fn trackable(metadata: &std::fs::Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() > 1 {
            return false;
        }
    }
    true
}

/// Open `path` without following a symlink, and only when it is still the
/// file `seen` describes.
fn open_seen(path: &Path, seen: &std::fs::Metadata) -> std::io::Result<std::fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let opened = file.metadata()?;
        if !trackable(&opened) || opened.dev() != seen.dev() || opened.ino() != seen.ino() {
            return Err(std::io::Error::other(
                "the file changed while it was opened",
            ));
        }
        Ok(file)
    }
    #[cfg(not(unix))]
    {
        let _ = seen;
        std::fs::File::open(path)
    }
}

impl Baseline {
    /// Hold `roots` until the returned lock is dropped, waiting at most
    /// `wait` for a root another holder has (or a folder inside or around
    /// it). Returns the lock on the roots it got and the roots it did not.
    /// Calls that change files in the same folder take turns this way, so
    /// each one's changes are told apart.
    pub async fn lock(&self, roots: Vec<PathBuf>, wait: Duration) -> (FolderLock, Vec<PathBuf>) {
        let deadline = tokio::time::Instant::now() + wait;
        let mut got = FolderLock {
            inner: self.inner.clone(),
            roots: Vec::new(),
        };
        let mut pending = roots;
        loop {
            let released = self.inner.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            {
                let mut held = locked(&self.inner.held);
                pending.retain(|root| {
                    if held.iter().any(|h| overlaps(h, root)) {
                        return true;
                    }
                    held.push(root.clone());
                    got.roots.push(root.clone());
                    false
                });
            }
            if pending.is_empty() {
                return (got, pending);
            }
            if tokio::time::timeout_at(deadline, released).await.is_err() {
                return (got, pending);
            }
        }
    }

    /// How many scans have run, for tests and diagnostics.
    pub fn scans(&self) -> usize {
        self.inner.scans.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Scan `root`, keep what it holds, and return what changed since the
    /// last scan of the same root. The first scan of a root returns nothing.
    /// A root found too large is not walked again for a few minutes.
    pub fn refresh(&self, root: &Path, limits: ScanLimits) -> Result<Vec<ScanChange>, FsError> {
        self.inner
            .scans
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.evict_idle(IDLE_FOR);
        let too_large = FsError::TooManyFiles {
            root: root.to_path_buf(),
            limit: limits.max_files,
        };
        if locked(&self.inner.too_large)
            .get(root)
            .is_some_and(|at| at.elapsed() < TOO_LARGE_FOR)
        {
            return Err(too_large);
        }
        #[cfg(test)]
        self.inner
            .walks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let paths = match walk(root, limits.max_files) {
            Ok(paths) => paths,
            Err(FsError::TooManyFiles { .. }) => {
                locked(&self.inner.too_large).insert(root.to_path_buf(), Instant::now());
                return Err(too_large);
            }
            Err(other) => return Err(other),
        };
        locked(&self.inner.too_large).remove(root);
        let previous = locked(&self.inner.roots).get(root).map(|f| f.files.clone());
        let started = SystemTime::now();
        let mut files = Files::new();
        for path in paths {
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !trackable(&metadata) {
                continue;
            }
            let (size, modified) = (metadata.len(), metadata.modified().ok());
            let known = previous.as_ref().and_then(|p| p.get(&path));
            let entry = match known {
                Some(entry) if !entry.racy && entry.size == size && entry.modified == modified => {
                    entry.clone()
                }
                _ => match self.read(&path, &metadata, started, limits) {
                    Ok(entry) => entry,
                    // Gone, replaced or unreadable since the walk listed it.
                    Err(_) => continue,
                },
            };
            files.insert(path, entry);
        }
        let changes = match &previous {
            Some(previous) => compare(previous, &files),
            None => Vec::new(),
        };
        drop(previous);
        locked(&self.inner.roots).insert(
            root.to_path_buf(),
            Folder {
                files: Arc::new(files),
                used: Instant::now(),
            },
        );
        locked(&self.inner.store).purge();
        Ok(changes)
    }

    /// Drop the snapshots of folders not scanned for `idle`, and the content
    /// only they used.
    fn evict_idle(&self, idle: Duration) {
        let evicted = {
            let mut roots = locked(&self.inner.roots);
            let before = roots.len();
            roots.retain(|_, folder| folder.used.elapsed() < idle);
            roots.len() != before
        };
        if evicted {
            locked(&self.inner.store).purge();
        }
    }

    fn read(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
        started: SystemTime,
        limits: ScanLimits,
    ) -> std::io::Result<Entry> {
        #[cfg(test)]
        self.inner
            .hashes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let modified = metadata.modified().ok();
        let racy = modified.is_none_or(|m| m + RACY_WINDOW >= started);
        let mut file = open_seen(path, metadata)?;
        if metadata.len() <= limits.max_content_bytes {
            // The size came from an earlier stat. A file that grew since (a
            // log being appended to) must not be read whole at any size, so
            // read one byte past the limit and, when it is there, fall back
            // to hashing the stream like any large file.
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            (&mut file)
                .take(limits.max_content_bytes + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limits.max_content_bytes {
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                let size = bytes.len() as u64 + std::io::copy(&mut file, &mut hasher)?;
                return Ok(Entry {
                    size,
                    modified,
                    digest: hasher.finalize().into(),
                    content: None,
                    racy,
                });
            }
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let binary = bytes[..bytes.len().min(BINARY_PROBE)].contains(&0);
            let size = bytes.len() as u64;
            let content = if binary {
                None
            } else {
                locked(&self.inner.store).keep(digest, bytes, limits.max_stored_bytes)
            };
            return Ok(Entry {
                size,
                modified,
                digest,
                content,
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

    const LONG: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn overlapping_folders_take_turns() {
        let baseline = Baseline::default();
        let (outer, _) = baseline.lock(vec![PathBuf::from("/w")], LONG).await;
        // A sibling folder is not held up.
        let (sibling, missed) = baseline.lock(vec![PathBuf::from("/x")], LONG).await;
        assert!(missed.is_empty());
        assert_eq!(sibling.roots(), [PathBuf::from("/x")]);
        let waiting = {
            let baseline = baseline.clone();
            tokio::spawn(async move { baseline.lock(vec![PathBuf::from("/w/sub")], LONG).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiting.is_finished());
        drop(outer);
        let (inner, missed) = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(missed.is_empty());
        assert_eq!(inner.roots(), [PathBuf::from("/w/sub")]);
    }

    #[tokio::test]
    async fn waiting_for_a_held_folder_is_bounded() {
        let baseline = Baseline::default();
        let (_outer, _) = baseline.lock(vec![PathBuf::from("/w")], LONG).await;
        let started = Instant::now();
        let (got, missed) = baseline
            .lock(
                vec![PathBuf::from("/w/sub"), PathBuf::from("/y")],
                Duration::from_millis(100),
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(got.roots(), [PathBuf::from("/y")]);
        assert_eq!(missed, [PathBuf::from("/w/sub")]);
        let (_, missed) = baseline
            .lock(vec![PathBuf::from("/w")], Duration::ZERO)
            .await;
        assert_eq!(missed, [PathBuf::from("/w")]);
    }

    #[test]
    fn a_folder_found_too_large_is_not_walked_again_soon() {
        let (_dir, root) = folder();
        for i in 0..5 {
            std::fs::write(root.join(format!("{i}.txt")), "x").unwrap();
        }
        let limits = ScanLimits {
            max_files: 3,
            ..Default::default()
        };
        let baseline = Baseline::default();
        assert!(baseline.refresh(&root, limits).is_err());
        assert!(baseline.refresh(&root, limits).is_err());
        assert_eq!(baseline.inner.walks.load(Ordering::SeqCst), 1);
        assert_eq!(baseline.scans(), 2);
    }

    #[test]
    fn content_is_stored_once_and_capped() {
        let (_one, first) = folder();
        let (_two, second) = folder();
        std::fs::write(first.join("a.txt"), "same").unwrap();
        std::fs::write(second.join("b.txt"), "same").unwrap();
        let baseline = Baseline::default();
        baseline.refresh(&first, ScanLimits::default()).unwrap();
        baseline.refresh(&second, ScanLimits::default()).unwrap();
        let store = locked(&baseline.inner.store);
        assert_eq!((store.blobs.len(), store.bytes), (1, 4));
        drop(store);

        let (_three, third) = folder();
        let baseline = Baseline::default();
        let limits = ScanLimits {
            max_stored_bytes: 4,
            ..Default::default()
        };
        baseline.refresh(&third, limits).unwrap();
        std::fs::write(third.join("small"), "abc").unwrap();
        std::fs::write(third.join("over"), "0123456789").unwrap();
        let changes = baseline.refresh(&third, limits).unwrap();
        let over = changes.iter().find(|c| c.path.ends_with("over")).unwrap();
        assert!(content(&over.after).is_none());
        assert_eq!(over.after.as_ref().unwrap().size, 10);
        let small = changes.iter().find(|c| c.path.ends_with("small")).unwrap();
        assert_eq!(content(&small.after), Some(&b"abc"[..]));
    }

    #[test]
    fn idle_folders_are_evicted() {
        let (_dir, root) = folder();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        baseline.evict_idle(Duration::ZERO);
        assert!(locked(&baseline.inner.roots).is_empty());
        assert_eq!(locked(&baseline.inner.store).bytes, 0);
        std::fs::write(root.join("a.txt"), "b").unwrap();
        // Evicted, so this is a first scan again.
        assert!(
            baseline
                .refresh(&root, ScanLimits::default())
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn links_are_not_tracked() {
        let (_dir, root) = folder();
        let baseline = Baseline::default();
        baseline.refresh(&root, ScanLimits::default()).unwrap();
        std::fs::write(root.join("a.txt"), "a").unwrap();
        std::fs::hard_link(root.join("a.txt"), root.join("b.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("plain.txt"), root.join("link.txt")).unwrap();
        std::fs::write(root.join("plain.txt"), "p").unwrap();
        let changes = baseline.refresh(&root, ScanLimits::default()).unwrap();
        let paths: Vec<_> = changes.iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, vec![root.join("plain.txt")]);
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

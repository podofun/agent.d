//! Binary file history. Callers own authorization and absolute path resolution.

use std::collections::HashMap;
use std::fs::{self, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{FsError, Snapshot};

/// Orders every change across all files, so a caller can ask whether a file
/// changed after a given moment.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn next_seq() -> u64 {
    SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Shared in-memory history; clone this handle across runtime reloads.
/// Operations on the same path serialize, including their filesystem commit.
#[derive(Clone, Default)]
pub struct History {
    files: Arc<Mutex<HashMap<PathBuf, Arc<Mutex<Timeline>>>>>,
    /// What each scope (a session, a run, or a connection) last read of a
    /// file, so an edit or a patch can prove it is based on what is there.
    reads: Reads,
}

type Reads = Arc<Mutex<HashMap<(String, PathBuf), Option<Stamp>>>>;

/// Fail unless `scope` read the file and it still matches that read.
fn check_read(reads: &Reads, scope: &str, path: &Path, now: &Option<Stamp>) -> Result<(), FsError> {
    let reads = reads.lock().map_err(|_| FsError::HistoryPoisoned)?;
    match reads.get(&(scope.to_owned(), path.to_path_buf())) {
        None => Err(FsError::NotRead(path.into())),
        Some(read) if read != now => Err(FsError::ChangedSinceRead(path.into())),
        Some(_) => Ok(()),
    }
}

/// Where `needle` starts in `text`, counting overlapping matches, up to `limit`.
fn occurrences(text: &str, needle: &str, limit: usize) -> usize {
    let mut count = 0;
    let mut start = 0;
    while count < limit {
        let Some(at) = text[start..].find(needle) else {
            break;
        };
        count += 1;
        let first = text[start + at..].chars().next().map_or(1, char::len_utf8);
        start += at + first;
    }
    count
}

/// Remember the file's current state as what `scope` has seen.
fn note_read(reads: &Reads, scope: &str, path: &Path) -> Result<(), FsError> {
    let stamp = Image::read(path)?.stamp();
    reads
        .lock()
        .map_err(|_| FsError::HistoryPoisoned)?
        .insert((scope.to_owned(), path.to_path_buf()), stamp);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Write,
    Append,
    Remove,
    /// A change made by a program an action ran, found by comparing the
    /// folder before and after the call.
    External,
    /// The file changed outside agent.d, for example in the user's editor.
    /// Undo and redo never cross it, so that edit is never discarded.
    Outside,
}

/// Which action, in which run, made a change.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Actor {
    pub action: String,
    pub run_id: Option<String>,
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
    pub actor: Option<Actor>,
    /// False when the content on either side was not kept (too large or
    /// binary), so undo and redo cannot reach past this change.
    pub restorable: bool,
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

#[derive(Clone)]
struct Stamp {
    digest: [u8; 32],
    /// The permissions to give the file when an undo brings it back after
    /// it was removed. `None` when not known, as for a file a program
    /// removed before agent.d saw it.
    permissions: Option<Permissions>,
}

/// Two stamps match when the contents are the same. Permissions are left
/// out: a program the agent runs, such as `chmod +x`, changes them without
/// changing what the file holds, and a write or an undo keeps whatever
/// permissions the file has on disk.
impl PartialEq for Stamp {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
    }
}

impl Eq for Stamp {}

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
        // Read through a handle confirmed to be this file, so a swap between
        // the metadata check above and the read cannot redirect it.
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut crate::open_verified(path)?, &mut bytes)?;
        Ok(Self {
            bytes: Some(bytes),
            permissions: Some(metadata.permissions()),
        })
    }

    fn stamp(&self) -> Option<Stamp> {
        self.bytes.as_ref().map(|bytes| Stamp {
            digest: Sha256::digest(bytes).into(),
            permissions: self.permissions.clone(),
        })
    }
}

struct Change {
    diff: FileDiff,
    before: Option<Stamp>,
    after: Option<Stamp>,
    created_dirs: Vec<PathBuf>,
    seq: u64,
}

/// Where `old` and `new` differ: the offset and the bytes removed and added.
fn splice(old: &[u8], new: &[u8]) -> (usize, Vec<u8>, Vec<u8>) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    (
        prefix,
        old[prefix..old.len() - suffix].to_vec(),
        new[prefix..new.len() - suffix].to_vec(),
    )
}

#[derive(Default)]
struct Timeline {
    changes: Vec<Change>,
    current: usize,
    redo: Vec<usize>,
    /// Every move of `current`, as its order among all changes and the
    /// revision it moved from, so a caller can see where a file was at a
    /// given moment.
    moves: Vec<(u64, usize)>,
}

/// A file whose history moved after a mark: the revision it is at now, and
/// its content then and now. `None` means absent; a side whose content could
/// not be rebuilt has `content: None`.
#[derive(Clone, Debug)]
pub struct Moved {
    pub path: PathBuf,
    pub revision: usize,
    pub before: Option<Snapshot>,
    pub after: Option<Snapshot>,
}

fn snapshot_of(bytes: Vec<u8>) -> Snapshot {
    Snapshot {
        size: bytes.len() as u64,
        digest: Sha256::digest(&bytes).into(),
        content: Some(bytes),
    }
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
        actor: Option<Actor>,
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
            Operation::External | Operation::Outside => {
                return Err(FsError::Unsupported(path.into()));
            }
        };
        let (prefix, removed, added) = splice(
            before.bytes.as_deref().unwrap_or_default(),
            bytes.as_deref().unwrap_or_default(),
        );
        let revision = Revision {
            id: self.changes.len() + 1,
            parent: self.current,
            operation,
            timestamp_ms: now_ms(),
            offset: prefix,
            removed_bytes: removed.len(),
            added_bytes: added.len(),
            before_size: before.bytes.as_ref().map(Vec::len),
            after_size: bytes.as_ref().map(Vec::len),
            actor,
            restorable: true,
        };
        let mut after = Image {
            bytes,
            permissions: before.permissions.clone(),
        };
        let created_dirs = commit(path, &mut after)?;
        let id = revision.id;
        let seq = next_seq();
        self.moves.push((seq, self.current));
        self.changes.push(Change {
            diff: FileDiff {
                revision,
                removed,
                added,
            },
            before: before_stamp,
            after: after.stamp(),
            created_dirs,
            seq,
        });
        self.current = id;
        self.redo.clear();
        Ok(id)
    }

    fn parent(&self, id: usize) -> usize {
        self.changes[id - 1].diff.revision.parent
    }

    fn restore(&mut self, path: &Path, target: usize) -> Result<usize, FsError> {
        let (mut image, backward) = self.image_at(path, target)?;
        if self.current != target {
            commit(path, &mut image)?;
            // Only remove directories this history created, and only if empty.
            for id in backward {
                for dir in &self.changes[id - 1].created_dirs {
                    let _ = fs::remove_dir(dir);
                }
            }
            self.moves.push((next_seq(), self.current));
            self.current = target;
        }
        Ok(target)
    }

    /// The file as it would be at revision `target`, built in memory from
    /// the file on disk, and the revisions walked back to get there.
    fn image_at(&self, path: &Path, target: usize) -> Result<(Image, Vec<usize>), FsError> {
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
        for &id in backward.iter().chain(&forward) {
            let revision = &self.changes[id - 1].diff.revision;
            if revision.operation == Operation::Outside {
                return Err(FsError::ChangedOutside(path.into()));
            }
            if !revision.restorable {
                return Err(FsError::NotRestorable(path.into()));
            }
        }
        for &id in &backward {
            apply(&mut image, &self.changes[id - 1], false);
        }
        for &id in forward.iter().rev() {
            apply(&mut image, &self.changes[id - 1], true);
        }
        Ok((image, backward))
    }

    /// The state the file has at revision `id`: `None` when unknown.
    fn stamp_at(&self, id: usize) -> Option<&Option<Stamp>> {
        match id {
            0 => self.changes.first().map(|c| &c.before),
            id => self.changes.get(id - 1).map(|c| &c.after),
        }
    }

    /// Where the file moved after `mark`, with its content then and now,
    /// when the file on disk is where this history left it.
    fn moved_since(&self, path: &Path, mark: u64) -> Option<Moved> {
        let &(_, from) = self.moves.iter().find(|(seq, _)| *seq > mark)?;
        if from == self.current {
            return None;
        }
        let now = Image::read(path).ok()?;
        if Some(&now.stamp()) != self.expected() {
            return None;
        }
        let before = match self.image_at(path, from) {
            Ok((image, _)) => image.bytes.map(snapshot_of),
            Err(_) => self.stamp_at(from)?.as_ref().map(|stamp| Snapshot {
                size: 0,
                digest: stamp.digest,
                content: None,
            }),
        };
        Some(Moved {
            path: path.to_path_buf(),
            revision: self.current,
            before,
            after: now.bytes.map(snapshot_of),
        })
    }

    /// The current revision when the file's expected state has `digest`
    /// (`None` for absent), so a change found on disk is already recorded.
    fn expected_revision(&self, digest: Option<[u8; 32]>) -> Option<usize> {
        let expected = self.expected()?;
        (expected.as_ref().map(|s| s.digest) == digest).then_some(self.current)
    }
}

impl Timeline {
    fn current_after_size(&self) -> Option<usize> {
        self.current
            .checked_sub(1)
            .and_then(|index| self.changes[index].diff.revision.after_size)
    }

    /// Add a change made on disk by something other than this history. The
    /// file already holds `after`. When this history changed the file after
    /// `before` was seen, its own changes are replayed from `before` so the
    /// new revision starts where they left off.
    fn record_external(
        &mut self,
        path: &Path,
        before: Option<Snapshot>,
        after: Option<Snapshot>,
        actor: Actor,
    ) -> Result<usize, FsError> {
        validate_ancestors(path)?;
        let permissions = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
            Ok(_) => return Err(FsError::Unsupported(path.into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let after_stamp = after.as_ref().map(|s| Stamp {
            digest: s.digest,
            permissions: permissions.clone(),
        });
        let before_digest = before.as_ref().map(|s| s.digest);
        let (before_stamp, before_bytes) = match self.expected() {
            None => (
                before.as_ref().map(|s| Stamp {
                    digest: s.digest,
                    permissions: permissions.clone(),
                }),
                before.as_ref().and_then(|s| s.content.clone()),
            ),
            Some(expected) if expected.as_ref().map(|s| s.digest) == before_digest => (
                expected.clone(),
                before.as_ref().and_then(|s| s.content.clone()),
            ),
            Some(expected) => (expected.clone(), self.replay(before.as_ref())),
        };
        let before_size = match (&before_stamp, &before_bytes) {
            (None, _) => None,
            (Some(_), Some(bytes)) => Some(bytes.len()),
            (Some(_), None) => self
                .current_after_size()
                .or(before.as_ref().map(|s| s.size as usize)),
        };
        let after_bytes = after.as_ref().and_then(|s| s.content.clone());
        let restorable = (before_stamp.is_none() || before_bytes.is_some())
            && (after.is_none() || after_bytes.is_some());
        let (offset, removed, added) = if restorable {
            splice(
                before_bytes.as_deref().unwrap_or_default(),
                after_bytes.as_deref().unwrap_or_default(),
            )
        } else {
            (0, Vec::new(), Vec::new())
        };
        let revision = Revision {
            id: self.changes.len() + 1,
            parent: self.current,
            operation: Operation::External,
            timestamp_ms: now_ms(),
            offset,
            removed_bytes: removed.len(),
            added_bytes: added.len(),
            before_size,
            after_size: after.as_ref().map(|s| s.size as usize),
            actor: Some(actor),
            restorable,
        };
        Ok(self.push(Change {
            diff: FileDiff {
                revision,
                removed,
                added,
            },
            before: before_stamp,
            after: after_stamp,
            created_dirs: Vec::new(),
            seq: next_seq(),
        }))
    }

    /// The content this history expects on disk now, rebuilt by replaying its
    /// own changes from `start`. `None` when that is not possible.
    fn replay(&self, start: Option<&Snapshot>) -> Option<Vec<u8>> {
        let start_digest = start.map(|s| s.digest);
        let mut chain = Vec::new();
        let mut id = self.current;
        loop {
            let change = self.changes.get(id.checked_sub(1)?)?;
            let revision = &change.diff.revision;
            if revision.operation == Operation::Outside || !revision.restorable {
                return None;
            }
            chain.push(id);
            if change.before.as_ref().map(|s| s.digest) == start_digest {
                break;
            }
            id = revision.parent;
        }
        let bytes = match start {
            Some(start) => Some(start.content.clone()?),
            None => None,
        };
        let mut image = Image {
            bytes,
            permissions: None,
        };
        for id in chain.iter().rev() {
            apply(&mut image, &self.changes[id - 1], true);
        }
        let bytes = image.bytes?;
        let expected = self.expected()?.as_ref()?;
        (<[u8; 32]>::from(Sha256::digest(&bytes)) == expected.digest).then_some(bytes)
    }

    /// Accept the file as it is on disk when it differs from what this
    /// history expects. A barrier marks the edit, so undo never discards it.
    fn acknowledge_outside(&mut self, path: &Path) -> Result<(), FsError> {
        let Some(expected) = self.expected().cloned() else {
            return Ok(());
        };
        let image = Image::read(path)?;
        let stamp = image.stamp();
        if stamp == expected {
            return Ok(());
        }
        let revision = Revision {
            id: self.changes.len() + 1,
            parent: self.current,
            operation: Operation::Outside,
            timestamp_ms: now_ms(),
            offset: 0,
            removed_bytes: 0,
            added_bytes: 0,
            before_size: self.current_after_size(),
            after_size: image.bytes.as_ref().map(Vec::len),
            actor: None,
            restorable: false,
        };
        self.push(Change {
            diff: FileDiff {
                revision,
                removed: Vec::new(),
                added: Vec::new(),
            },
            before: expected,
            after: stamp,
            created_dirs: Vec::new(),
            seq: next_seq(),
        });
        Ok(())
    }

    /// [`Timeline::acknowledge_outside`], only when the file on disk holds
    /// the content with `digest`.
    fn acknowledge_seen(&mut self, path: &Path, digest: [u8; 32]) -> Result<(), FsError> {
        match self.expected() {
            None => return Ok(()),
            Some(Some(expected)) if expected.digest == digest => return Ok(()),
            Some(_) => {}
        }
        if Image::read(path)?.stamp().map(|s| s.digest) != Some(digest) {
            return Ok(());
        }
        self.acknowledge_outside(path)
    }

    fn push(&mut self, change: Change) -> usize {
        let id = change.diff.revision.id;
        self.moves.push((change.seq, self.current));
        self.changes.push(change);
        self.current = id;
        self.redo.clear();
        id
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
        // A file that exists keeps its permissions; one brought back gets
        // the permissions it had.
        if image.bytes.is_none() {
            image.permissions = stamp.permissions.clone();
        }
        let bytes = image.bytes.get_or_insert_with(Vec::new);
        bytes.splice(
            diff.revision.offset..diff.revision.offset + remove.len(),
            insert.iter().copied(),
        );
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
        let builder = new_file_builder(image.permissions.is_none());
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

/// The temporary-file builder for a write. A brand-new file on Unix starts
/// from mode 0o666 (narrowed by the umask), like a plain `File::create`.
fn new_file_builder(new_file: bool) -> tempfile::Builder<'static, 'static> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut builder = tempfile::Builder::new();
        if new_file {
            builder.permissions(Permissions::from_mode(0o666));
        }
        builder
    }
    #[cfg(not(unix))]
    {
        let _ = new_file;
        tempfile::Builder::new()
    }
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

    /// Record that `scope` has read the file as it is now.
    pub async fn record_read(&self, scope: &str, path: &Path) -> Result<(), FsError> {
        let (reads, scope) = (self.reads.clone(), scope.to_owned());
        self.with_file(path, move |_, p| note_read(&reads, &scope, p))
            .await
    }

    /// Replace `old` with `new` in a text file that `scope` has read and that
    /// has not changed since. Without `replace_all`, `old` must appear once.
    /// A CRLF file keeps its endings when `old` and `new` use plain newlines.
    pub async fn edit(
        &self,
        scope: &str,
        path: &Path,
        old: &str,
        new: &str,
        replace_all: bool,
        actor: Option<Actor>,
    ) -> Result<usize, FsError> {
        let (reads, scope) = (self.reads.clone(), scope.to_owned());
        let (old, new) = (old.to_owned(), new.to_owned());
        self.with_file(path, move |t, p| {
            let image = Image::read(p)?;
            check_read(&reads, &scope, p, &image.stamp())?;
            let bytes = image.bytes.ok_or_else(|| FsError::NotFound(p.into()))?;
            let text = String::from_utf8(bytes).map_err(|_| FsError::InvalidUtf8(p.into()))?;
            let (old, new) = if text.contains("\r\n") && !old.contains('\r') {
                (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
            } else {
                (old, new)
            };
            let count = if old.is_empty() {
                0
            } else {
                occurrences(&text, &old, if replace_all { usize::MAX } else { 2 })
            };
            if count == 0 || (count > 1 && !replace_all) {
                return Err(FsError::EditNotFound {
                    path: p.into(),
                    count,
                });
            }
            let changed = if replace_all {
                text.replace(&old, &new)
            } else {
                text.replacen(&old, &new, 1)
            };
            let revision = t.mutate(p, Operation::Write, changed.into_bytes(), actor)?;
            note_read(&reads, &scope, p)?;
            Ok(revision)
        })
        .await
    }

    /// Record that `scope` was handed `bytes` as the content of `path`, so a
    /// later edit or patch is checked against what the caller actually saw.
    /// When the file still holds `bytes` but changed outside agent.d, that
    /// edit is accepted as [`History::acknowledge_outside`] does, since the
    /// caller has now seen it. Best effort: a file history cannot track (a
    /// hard link, say) is simply not recorded, and a later edit of it asks
    /// for a read.
    pub async fn record_read_bytes(&self, scope: &str, path: &Path, bytes: &[u8]) {
        let Ok(metadata) = fs::metadata(path) else {
            return;
        };
        let stamp = Stamp {
            digest: Sha256::digest(bytes).into(),
            permissions: Some(metadata.permissions()),
        };
        let seen = stamp.digest;
        if let Ok(mut reads) = self.reads.lock() {
            reads.insert((scope.to_owned(), path.to_path_buf()), Some(stamp));
        }
        let _ = self
            .with_file(path, move |t, p| t.acknowledge_seen(p, seen))
            .await;
    }

    /// Apply a patch to several files, all or nothing. Files it modifies or
    /// removes must have been read by `scope` and left unchanged since; a
    /// file it creates must not exist yet. Every hunk of every file is applied
    /// in memory first, so a hunk that does not apply changes nothing. Each
    /// file is checked again just before it is written, and if a write fails
    /// the files already written are put back as they were.
    pub async fn patch(
        &self,
        scope: &str,
        files: &[(PathBuf, crate::FilePatch)],
        actor: Option<Actor>,
    ) -> Result<Vec<(PathBuf, usize)>, FsError> {
        self.patch_with_pause(scope, files, actor, || {}).await
    }

    /// [`History::patch`], running `pause` between planning and writing.
    /// Tests use it to change a file in that window.
    async fn patch_with_pause(
        &self,
        scope: &str,
        files: &[(PathBuf, crate::FilePatch)],
        actor: Option<Actor>,
        pause: impl FnOnce(),
    ) -> Result<Vec<(PathBuf, usize)>, FsError> {
        let mut seen = std::collections::HashSet::new();
        for (path, _) in files {
            if !seen.insert(path.clone()) {
                return Err(FsError::DuplicatePath(path.clone()));
            }
        }
        let mut planned = Vec::new();
        for (path, patch) in files {
            let (reads, scope, patch) = (self.reads.clone(), scope.to_owned(), patch.clone());
            let plan = self
                .with_file(path, move |_, p| {
                    let image = Image::read(p)?;
                    let stamp = image.stamp();
                    let text = if patch.old_path.is_none() {
                        if image.bytes.is_some() {
                            return Err(FsError::AlreadyExists(p.into()));
                        }
                        String::new()
                    } else {
                        check_read(&reads, &scope, p, &stamp)?;
                        let bytes = image.bytes.ok_or_else(|| FsError::NotFound(p.into()))?;
                        String::from_utf8(bytes).map_err(|_| FsError::InvalidUtf8(p.into()))?
                    };
                    let changed = crate::apply_patch(&text, &patch)?;
                    let content = if patch.new_path.is_none() {
                        if !changed.is_empty() {
                            return Err(FsError::RemoveLeavesLines(p.into()));
                        }
                        None
                    } else {
                        Some(changed.into_bytes())
                    };
                    Ok((stamp, content))
                })
                .await?;
            planned.push((path.clone(), plan));
        }
        pause();
        let earlier_reads: Vec<(PathBuf, Option<Option<Stamp>>)> = {
            let reads = self.reads.lock().map_err(|_| FsError::HistoryPoisoned)?;
            planned
                .iter()
                .map(|(path, _)| {
                    (
                        path.clone(),
                        reads.get(&(scope.to_owned(), path.clone())).cloned(),
                    )
                })
                .collect()
        };
        let mut done: Vec<(PathBuf, usize, usize)> = Vec::new();
        let mut failure = None;
        for (path, (stamp, content)) in planned {
            let (reads, scope, actor) = (self.reads.clone(), scope.to_owned(), actor.clone());
            let outcome = self
                .with_file(&path, move |t, p| {
                    if Image::read(p)?.stamp() != stamp {
                        return Err(FsError::ChangedSinceRead(p.into()));
                    }
                    let previous = t.current;
                    let revision = match content {
                        Some(bytes) => t.mutate(p, Operation::Write, bytes, actor)?,
                        None => t.mutate(p, Operation::Remove, Vec::new(), actor)?,
                    };
                    let _ = note_read(&reads, &scope, p);
                    Ok((revision, previous))
                })
                .await;
            match outcome {
                Ok((revision, previous)) => done.push((path, revision, previous)),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        let Some(error) = failure else {
            return Ok(done
                .into_iter()
                .map(|(path, revision, _)| (path, revision))
                .collect());
        };
        let mut still_changed = Vec::new();
        for (path, _, previous) in done.iter().rev() {
            if self.restore(path, *previous).await.is_err() {
                still_changed.push(path.clone());
            }
        }
        if let Ok(mut reads) = self.reads.lock() {
            for (path, earlier) in earlier_reads {
                let key = (scope.to_owned(), path);
                match earlier {
                    Some(stamp) => reads.insert(key, stamp),
                    None => reads.remove(&key),
                };
            }
        }
        if still_changed.is_empty() {
            Err(error)
        } else {
            Err(FsError::PatchPartlyApplied {
                cause: Box::new(error),
                still_changed,
            })
        }
    }

    pub async fn write(
        &self,
        path: &Path,
        bytes: &[u8],
        actor: Option<Actor>,
    ) -> Result<usize, FsError> {
        let bytes = bytes.to_vec();
        self.with_file(path, move |t, p| {
            t.mutate(p, Operation::Write, bytes, actor)
        })
        .await
    }

    pub async fn append(
        &self,
        path: &Path,
        bytes: &[u8],
        actor: Option<Actor>,
    ) -> Result<usize, FsError> {
        let bytes = bytes.to_vec();
        self.with_file(path, move |t, p| {
            t.mutate(p, Operation::Append, bytes, actor)
        })
        .await
    }

    pub async fn remove(&self, path: &Path, actor: Option<Actor>) -> Result<usize, FsError> {
        self.with_file(path, move |t, p| {
            t.mutate(p, Operation::Remove, Vec::new(), actor)
        })
        .await
    }

    /// Record a change something other than this history made to `path`,
    /// which already holds `after`. `None` means absent. The change can be
    /// undone when both sides kept their content.
    pub async fn record_external(
        &self,
        path: &Path,
        before: Option<&Snapshot>,
        after: Option<&Snapshot>,
        actor: Actor,
    ) -> Result<usize, FsError> {
        let (before, after) = (before.cloned(), after.cloned());
        self.with_file(path, move |t, p| t.record_external(p, before, after, actor))
            .await
    }

    /// Accept `path` as it is now when it changed outside agent.d, so the
    /// next write succeeds. Undo still refuses to go past that edit.
    pub async fn acknowledge_outside(&self, path: &Path) -> Result<(), FsError> {
        self.with_file(path, |t, p| t.acknowledge_outside(p)).await
    }

    /// A point in the order of changes. Pass it to
    /// [`History::recorded_since`] to ask about changes made after it.
    pub fn mark(&self) -> u64 {
        SEQ.load(Ordering::SeqCst)
    }

    /// The current revision of `path` when the file history expects it to
    /// hold content with `digest` (`None` for absent). A change found on disk
    /// that matches is already recorded, whether by a write, an undo, a redo
    /// or a restore.
    pub async fn expected_revision(
        &self,
        path: &Path,
        digest: Option<[u8; 32]>,
    ) -> Result<Option<usize>, FsError> {
        self.with_file(path, move |t, _| Ok(t.expected_revision(digest)))
            .await
    }

    /// Every file under `roots` whose history moved after `mark`, with its
    /// content then and now. Files that changed on disk since are left out.
    pub async fn moved_since(&self, mark: u64, roots: &[PathBuf]) -> Vec<Moved> {
        let timelines = self.timelines_in(roots);
        tokio::task::spawn_blocking(move || {
            timelines
                .into_iter()
                .filter_map(|(path, timeline)| timeline.lock().ok()?.moved_since(&path, mark))
                .collect()
        })
        .await
        .unwrap_or_default()
    }

    /// Accept every file under `roots` that changed outside agent.d, as
    /// [`History::acknowledge_outside`] does for one file.
    pub async fn acknowledge_outside_in(&self, roots: &[PathBuf]) {
        let timelines = self.timelines_in(roots);
        let _ = tokio::task::spawn_blocking(move || {
            for (path, timeline) in timelines {
                if let Ok(mut timeline) = timeline.lock() {
                    let _ = timeline.acknowledge_outside(&path);
                }
            }
        })
        .await;
    }

    fn timelines_in(&self, roots: &[PathBuf]) -> Vec<(PathBuf, Arc<Mutex<Timeline>>)> {
        let Ok(files) = self.files.lock() else {
            return Vec::new();
        };
        files
            .iter()
            .filter(|(path, _)| roots.iter().any(|root| path.starts_with(root)))
            .map(|(path, timeline)| (path.clone(), timeline.clone()))
            .collect()
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
    async fn edit_requires_a_read() {
        let (_dir, path) = temp();
        std::fs::write(&path, "abc").unwrap();
        let history = History::default();
        let error = history
            .edit("session:s", &path, "b", "B", false, None)
            .await
            .unwrap_err();
        assert!(matches!(error, FsError::NotRead(_)), "{error:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc");
    }

    #[tokio::test]
    async fn edit_after_read_changes_and_records_history() {
        let (_dir, path) = temp();
        std::fs::write(&path, "abc").unwrap();
        let history = History::default();
        history.record_read("session:s", &path).await.unwrap();
        history
            .edit("session:s", &path, "b", "B", false, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aBc");
        history
            .edit("session:s", &path, "c", "C", false, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aBC");
        assert_eq!(history.history(&path).await.unwrap().revisions.len(), 2);
        history.undo(&path).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "aBc");
    }

    #[tokio::test]
    async fn edit_refuses_after_an_outside_change() {
        let (_dir, path) = temp();
        std::fs::write(&path, "abc").unwrap();
        let history = History::default();
        history.record_read("session:s", &path).await.unwrap();
        std::fs::write(&path, "abcd").unwrap();
        let error = history
            .edit("session:s", &path, "b", "B", false, None)
            .await
            .unwrap_err();
        assert!(matches!(error, FsError::ChangedSinceRead(_)), "{error:?}");
        assert!(error.to_string().contains("read it again"), "{error}");
    }

    #[tokio::test]
    async fn edit_counts_matches() {
        let (_dir, path) = temp();
        std::fs::write(&path, "x x").unwrap();
        let history = History::default();
        history.record_read("session:s", &path).await.unwrap();
        let error = history
            .edit("session:s", &path, "x", "y", false, None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, FsError::EditNotFound { count: 2, .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("appears 2 times"), "{error}");
        let error = history
            .edit("session:s", &path, "q", "y", false, None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, FsError::EditNotFound { count: 0, .. }),
            "{error:?}"
        );
        history
            .edit("session:s", &path, "x", "y", true, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y y");
    }

    #[tokio::test]
    async fn edit_keeps_crlf_endings() {
        let (_dir, path) = temp();
        std::fs::write(&path, "a\r\nb\r\n").unwrap();
        let history = History::default();
        history.record_read("session:s", &path).await.unwrap();
        history
            .edit("session:s", &path, "a\nb", "a\nc", false, None)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\r\nc\r\n");
    }

    fn file_patch(path: &str, text: &str) -> (PathBuf, crate::FilePatch) {
        let mut parsed = crate::parse_patch(text).unwrap();
        (PathBuf::from(path), parsed.remove(0))
    }

    #[tokio::test]
    async fn patch_applies_every_file_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b) = (root.join("a.txt"), root.join("b.txt"));
        std::fs::write(&a, "1\n").unwrap();
        std::fs::write(&b, "x\n").unwrap();
        let history = History::default();
        history.record_read("s", &a).await.unwrap();
        history.record_read("s", &b).await.unwrap();
        let files = vec![
            (
                a.clone(),
                file_patch("a.txt", "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-1\n+2\n").1,
            ),
            (
                b.clone(),
                file_patch(
                    "b.txt",
                    "--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-nope\n+y\n",
                )
                .1,
            ),
        ];
        let error = history.patch("s", &files, None).await.unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("hunk 1 of `b.txt` does not apply"),
            "{message}"
        );
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "x\n");
    }

    #[tokio::test]
    async fn patch_creates_modifies_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (new, old, edit) = (
            root.join("new.txt"),
            root.join("old.txt"),
            root.join("edit.txt"),
        );
        std::fs::write(&old, "bye\n").unwrap();
        std::fs::write(&edit, "a\n").unwrap();
        let history = History::default();
        history.record_read("s", &old).await.unwrap();
        history.record_read("s", &edit).await.unwrap();
        let files = vec![
            (
                new.clone(),
                file_patch(
                    "new.txt",
                    "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hi\n",
                )
                .1,
            ),
            (
                old.clone(),
                file_patch(
                    "old.txt",
                    "--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-bye\n",
                )
                .1,
            ),
            (
                edit.clone(),
                file_patch(
                    "edit.txt",
                    "--- a/edit.txt\n+++ b/edit.txt\n@@ -1 +1 @@\n-a\n+b\n",
                )
                .1,
            ),
        ];
        let done = history.patch("s", &files, None).await.unwrap();
        assert_eq!(done.len(), 3);
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "hi\n");
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(&edit).unwrap(), "b\n");
        history.undo(&old).await.unwrap();
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "bye\n");
    }

    #[tokio::test]
    async fn patch_needs_reads_and_refuses_to_create_over_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("a.txt");
        std::fs::write(&a, "1\n").unwrap();
        let history = History::default();
        let modify = vec![(
            a.clone(),
            file_patch("a.txt", "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-1\n+2\n").1,
        )];
        let error = history.patch("s", &modify, None).await.unwrap_err();
        assert!(matches!(error, FsError::NotRead(_)), "{error:?}");
        let create = vec![(
            a.clone(),
            file_patch("a.txt", "--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+x\n").1,
        )];
        let error = history.patch("s", &create, None).await.unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1\n");
    }

    #[tokio::test]
    async fn a_read_is_stamped_from_the_bytes_returned() {
        let (_dir, path) = temp();
        std::fs::write(&path, "new").unwrap();
        let history = History::default();
        // The caller was handed "old", so its read does not cover "new".
        history.record_read_bytes("s", &path, b"old").await;
        let error = history
            .edit("s", &path, "new", "x", false, None)
            .await
            .unwrap_err();
        assert!(matches!(error, FsError::ChangedSinceRead(_)), "{error:?}");
    }

    #[tokio::test]
    async fn the_same_file_twice_in_one_patch_is_refused() {
        let (_dir, path) = temp();
        std::fs::write(&path, "1\n").unwrap();
        let history = History::default();
        history.record_read("s", &path).await.unwrap();
        let section = file_patch("a", "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-1\n+2\n").1;
        let error = history
            .patch(
                "s",
                &[(path.clone(), section.clone()), (path.clone(), section)],
                None,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("more than once"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\n");
    }

    #[tokio::test]
    async fn a_change_between_planning_and_writing_is_not_overwritten() {
        let (_dir, path) = temp();
        std::fs::write(&path, "1\n").unwrap();
        let history = History::default();
        history.record_read("s", &path).await.unwrap();
        let section = file_patch("a", "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-1\n+2\n").1;
        let sneaky = path.clone();
        let error = history
            .patch_with_pause("s", &[(path.clone(), section)], None, move || {
                std::fs::write(&sneaky, "someone else\n").unwrap();
            })
            .await
            .unwrap_err();
        assert!(matches!(error, FsError::ChangedSinceRead(_)), "{error:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else\n");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_write_rolls_back_cleanly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let a = root.join("a.txt");
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        let b = locked.join("b.txt");
        std::fs::write(&a, "1\n").unwrap();
        std::fs::write(&b, "x\n").unwrap();
        let history = History::default();
        history.record_read("s", &a).await.unwrap();
        history.record_read("s", &b).await.unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let files = vec![
            (
                a.clone(),
                file_patch("a.txt", "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-1\n+2\n").1,
            ),
            (
                b.clone(),
                file_patch("b.txt", "--- a/b.txt\n+++ b/b.txt\n@@ -1 +1 @@\n-x\n+y\n").1,
            ),
        ];
        let result = history.patch("s", &files, None).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "1\n");
        // The rollback leaves nothing to redo, and the read still counts.
        assert!(matches!(history.redo(&a).await, Err(FsError::NoRedo)));
        history.edit("s", &a, "1", "3", false, None).await.unwrap();
    }

    #[tokio::test]
    async fn overlapping_matches_count_as_ambiguous() {
        let (_dir, path) = temp();
        std::fs::write(&path, "}\n}\n}").unwrap();
        let history = History::default();
        history.record_read("s", &path).await.unwrap();
        let error = history
            .edit("s", &path, "}\n}", "x", false, None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, FsError::EditNotFound { count: 2, .. }),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_outside_change_to_a_file_agentd_wrote_says_what_to_do() {
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"a", None).await.unwrap();
        std::fs::write(&path, "b").unwrap();
        history.record_read("s", &path).await.unwrap();
        let error = history
            .edit("s", &path, "b", "c", false, None)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("changed outside agent.d"), "{message}");
        assert!(!message.contains("runtime history:"), "{message}");
    }

    #[tokio::test]
    async fn scopes_do_not_share_reads() {
        let (_dir, path) = temp();
        std::fs::write(&path, "abc").unwrap();
        let history = History::default();
        history.record_read("session:a", &path).await.unwrap();
        let error = history
            .edit("session:b", &path, "b", "B", false, None)
            .await
            .unwrap_err();
        assert!(matches!(error, FsError::NotRead(_)), "{error:?}");
    }

    #[tokio::test]
    async fn binary_create_append_delete_undo_redo() {
        let (_dir, path) = temp();
        let history = History::default();
        assert_eq!(history.write(&path, &[0, 255, 128], None).await.unwrap(), 1);
        assert_eq!(history.append(&path, &[254, 0], None).await.unwrap(), 2);
        assert_eq!(history.remove(&path, None).await.unwrap(), 3);
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
        history.write(&path, b"first", None).await.unwrap();
        history.write(&path, b"second", None).await.unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(history.write(&path, b"branch", None).await.unwrap(), 3);
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
        history.write(&path, b"", None).await.unwrap();
        history.write(&path, b"", None).await.unwrap();
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
        history.write(&path, &bytes, None).await.unwrap();
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
        history.write(&path, b"tracked", None).await.unwrap();
        fs::write(&path, b"outside edit").unwrap();
        assert!(matches!(
            history.undo(&path).await,
            Err(FsError::Conflict(_))
        ));
        assert!(matches!(
            history.write(&path, b"lost", None).await,
            Err(FsError::Conflict(_))
        ));
        assert!(matches!(
            history.append(&path, b"lost", None).await,
            Err(FsError::Conflict(_))
        ));
        assert!(matches!(
            history.remove(&path, None).await,
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
        assert!(history.remove(&path, None).await.is_err());
        assert!(history.restore(&path, 1).await.is_err());
        assert!(history.diff(&path, 0).await.is_err());
        fs::create_dir(&path).unwrap();
        assert!(history.write(&path, b"x", None).await.is_err());
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
            history.write(&path, &bytes, None).await.unwrap();
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
                history.append(&path, &[byte], None).await.unwrap()
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
        history.write(&path, b"x", None).await.unwrap();
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
        history.write(&path, b"new", None).await.unwrap();
        history.remove(&path, None).await.unwrap();
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
            history.write(&link, b"bad", None).await,
            Err(FsError::Unsupported(_))
        ));
        fs::remove_file(&link).unwrap();
        fs::hard_link(&path, &link).unwrap();
        assert!(matches!(
            history.write(&path, b"bad", None).await,
            Err(FsError::Unsupported(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }

    fn snapshot(bytes: &[u8], keep: bool) -> crate::Snapshot {
        crate::Snapshot {
            size: bytes.len() as u64,
            digest: Sha256::digest(bytes).into(),
            content: keep.then(|| bytes.to_vec()),
        }
    }

    fn actor(action: &str) -> Option<Actor> {
        Some(Actor {
            action: action.into(),
            run_id: Some("run-1".into()),
        })
    }

    #[tokio::test]
    async fn external_changes_are_undoable() {
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"a", None).await.unwrap();
        fs::write(&path, b"b").unwrap();
        let revision = history
            .record_external(
                &path,
                Some(&snapshot(b"a", true)),
                Some(&snapshot(b"b", true)),
                actor("shell.run").unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revision, 2);
        let log = history.history(&path).await.unwrap();
        assert!(matches!(log.revisions[1].operation, Operation::External));
        assert!(log.revisions[1].restorable);
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a");
        history.redo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"b");
    }

    #[tokio::test]
    async fn external_creations_and_removals_are_undoable() {
        let (_dir, path) = temp();
        let history = History::default();
        fs::write(&path, b"new").unwrap();
        history
            .record_external(
                &path,
                None,
                Some(&snapshot(b"new", true)),
                actor("shell.run").unwrap(),
            )
            .await
            .unwrap();
        fs::remove_file(&path).unwrap();
        history
            .record_external(
                &path,
                Some(&snapshot(b"new", true)),
                None,
                actor("shell.run").unwrap(),
            )
            .await
            .unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        history.undo(&path).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn an_external_change_after_a_write_in_the_same_call_builds_on_it() {
        let (_dir, path) = temp();
        fs::write(&path, b"original\n").unwrap();
        let history = History::default();
        history.write(&path, b"written\n", None).await.unwrap();
        fs::write(&path, b"formatted\n").unwrap();
        // The caller only knows the file as it was before both changes.
        history
            .record_external(
                &path,
                Some(&snapshot(b"original\n", true)),
                Some(&snapshot(b"formatted\n", true)),
                actor("files.format").unwrap(),
            )
            .await
            .unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"written\n");
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"original\n");
    }

    #[tokio::test]
    async fn outside_edits_unblock_writes_but_not_undo() {
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"a", None).await.unwrap();
        fs::write(&path, b"b").unwrap();
        assert!(matches!(
            history.write(&path, b"c", None).await,
            Err(FsError::Conflict(_))
        ));
        history.acknowledge_outside(&path).await.unwrap();
        history.write(&path, b"c", None).await.unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"b");
        let error = history.undo(&path).await.unwrap_err();
        assert!(matches!(error, FsError::ChangedOutside(_)), "{error:?}");
        assert!(
            error.to_string().contains("changed outside agent.d"),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"b");
        let error = history.restore(&path, 1).await.unwrap_err();
        assert!(matches!(error, FsError::ChangedOutside(_)), "{error:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_permission_change_is_not_an_outside_edit() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"a", None).await.unwrap();
        history.record_read("session:s", &path).await.unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o755)).unwrap();
        history
            .edit("session:s", &path, "a", "b", false, None)
            .await
            .unwrap();
        history.write(&path, b"c", None).await.unwrap();
        history.undo(&path).await.unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"a");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        history.remove(&path, None).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_fresh_read_accepts_an_outside_edit_but_undo_keeps_it() {
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"a", None).await.unwrap();
        fs::write(&path, b"b").unwrap();
        assert!(matches!(
            history.remove(&path, None).await,
            Err(FsError::Conflict(_))
        ));
        history.record_read_bytes("session:s", &path, b"b").await;
        history.write(&path, b"c", None).await.unwrap();
        history.undo(&path).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"b");
        let error = history.undo(&path).await.unwrap_err();
        assert!(matches!(error, FsError::ChangedOutside(_)), "{error:?}");
        assert_eq!(fs::read(&path).unwrap(), b"b");
    }

    #[tokio::test]
    async fn a_read_of_other_bytes_accepts_nothing() {
        let (_dir, path) = temp();
        let history = History::default();
        history.write(&path, b"a", None).await.unwrap();
        fs::write(&path, b"b").unwrap();
        history
            .record_read_bytes("session:s", &path, b"stale")
            .await;
        assert!(matches!(
            history.write(&path, b"c", None).await,
            Err(FsError::Conflict(_))
        ));
        assert_eq!(fs::read(&path).unwrap(), b"b");
    }

    #[tokio::test]
    async fn acknowledging_an_unchanged_file_records_nothing() {
        let (_dir, path) = temp();
        let history = History::default();
        history.acknowledge_outside(&path).await.unwrap();
        history.write(&path, b"a", None).await.unwrap();
        history.acknowledge_outside(&path).await.unwrap();
        assert_eq!(history.history(&path).await.unwrap().revisions.len(), 1);
    }

    #[tokio::test]
    async fn unrestorable_changes_refuse_undo() {
        let (_dir, path) = temp();
        fs::write(&path, b"\0big").unwrap();
        let history = History::default();
        history
            .record_external(
                &path,
                None,
                Some(&snapshot(b"\0big", false)),
                actor("shell.run").unwrap(),
            )
            .await
            .unwrap();
        let log = history.history(&path).await.unwrap();
        assert!(!log.revisions[0].restorable);
        assert_eq!(log.revisions[0].after_size, Some(4));
        let error = history.undo(&path).await.unwrap_err();
        assert!(matches!(error, FsError::NotRestorable(_)), "{error:?}");
        assert!(error.to_string().contains("cannot be undone"), "{error}");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn revisions_name_their_actor() {
        let (_dir, path) = temp();
        let history = History::default();
        history
            .write(&path, b"a", actor("files.write"))
            .await
            .unwrap();
        let log = history.history(&path).await.unwrap();
        let actor = log.revisions[0].actor.as_ref().unwrap();
        assert_eq!(actor.action, "files.write");
        assert_eq!(actor.run_id.as_deref(), Some("run-1"));
    }

    fn digest(bytes: &[u8]) -> Option<[u8; 32]> {
        Some(Sha256::digest(bytes).into())
    }

    #[tokio::test]
    async fn the_expected_state_names_the_current_revision() {
        let (_dir, path) = temp();
        let history = History::default();
        assert_eq!(history.expected_revision(&path, None).await.unwrap(), None);
        history.write(&path, b"a", None).await.unwrap();
        history.write(&path, b"b", None).await.unwrap();
        assert_eq!(
            history
                .expected_revision(&path, digest(b"b"))
                .await
                .unwrap(),
            Some(2)
        );
        history.undo(&path).await.unwrap();
        assert_eq!(
            history
                .expected_revision(&path, digest(b"a"))
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            history
                .expected_revision(&path, digest(b"b"))
                .await
                .unwrap(),
            None
        );
        history.undo(&path).await.unwrap();
        assert_eq!(
            history.expected_revision(&path, None).await.unwrap(),
            Some(0)
        );
    }

    #[tokio::test]
    async fn moves_since_a_mark_carry_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (path, other) = (root.join("a.txt"), root.join("b.txt"));
        let history = History::default();
        history.write(&path, b"one", None).await.unwrap();
        history.write(&other, b"other", None).await.unwrap();
        let mark = history.mark();
        history.write(&path, b"two", None).await.unwrap();
        history.write(&path, b"three", None).await.unwrap();
        let moved = history.moved_since(mark, std::slice::from_ref(&root)).await;
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert_eq!(moved[0].path, path);
        assert_eq!(moved[0].revision, 3);
        let content = |s: &Option<Snapshot>| s.as_ref().and_then(|s| s.content.clone());
        assert_eq!(content(&moved[0].before), Some(b"one".to_vec()));
        assert_eq!(content(&moved[0].after), Some(b"three".to_vec()));

        let mark = history.mark();
        history.undo(&path).await.unwrap();
        let moved = history.moved_since(mark, std::slice::from_ref(&root)).await;
        assert_eq!(content(&moved[0].before), Some(b"three".to_vec()));
        assert_eq!(content(&moved[0].after), Some(b"two".to_vec()));

        let mark = history.mark();
        history.undo(&path).await.unwrap();
        history.redo(&path).await.unwrap();
        assert!(
            history
                .moved_since(mark, std::slice::from_ref(&root))
                .await
                .is_empty()
        );
        assert!(
            history
                .moved_since(0, &[root.join("elsewhere")])
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn outside_edits_under_folders_are_accepted_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("a.txt");
        let history = History::default();
        history.write(&path, b"a", None).await.unwrap();
        fs::write(&path, b"edited").unwrap();
        history
            .acknowledge_outside_in(std::slice::from_ref(&root))
            .await;
        history.write(&path, b"c", None).await.unwrap();
    }
}

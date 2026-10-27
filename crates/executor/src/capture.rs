//! Capture every file an action call changes, however it changed it.
//!
//! A call that may run programs (it holds a `shell.*` grant, or it is a
//! runner's tool call) has the folders it may write scanned before and after
//! it runs. Anything that changed before the call was changed outside
//! agent.d, so file history accepts it without reporting it. What changed
//! during the call is reported: changes file history already expects (a
//! `ctx.fs` write, undo, redo or restore) keep their revision, and anything
//! else is recorded as an external revision.
//!
//! A call that cannot run programs can only change files through `ctx.fs`,
//! which file history records, so its changes are read from history and no
//! folder is scanned.
//!
//! Calls that write the same folder take turns through a folder lock held
//! for the whole call, so each call's changes are its own. Waiting for the
//! lock is bounded: a call that does not get a folder in time leaves it
//! untracked and says so on its trace event. A call nested inside another
//! (an action that starts a runner whose tools write files) never waits.
//! When the enclosing call scans its folders, the nested call leaves them
//! to it. When the enclosing call does not scan and the nested call may run
//! programs, the nested call scans the enclosing call's folders itself,
//! without taking the lock its enclosing call already holds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use agentd_ai::{ChangeKind, FileChange};
use agentd_fs::{Actor, Baseline, FolderLock, FsError, History, ScanLimits, Snapshot};
use agentd_permissions::{Caller, PermissionSet};

/// A diff longer than this many lines is cut, with a final line saying how
/// many were left out.
const MAX_DIFF_LINES: usize = 400;

/// How long a call waits for a folder another call is changing.
pub(crate) const LOCK_WAIT: Duration = Duration::from_secs(2);

/// The note a call's trace carries when a folder was too large to scan.
const TOO_LARGE: &str = "the folder was too large to track changes in";

/// The note a call's trace carries when another call held its folder.
const BUSY: &str = "another call was changing this folder, so changes were not tracked";

/// File history and the folder baseline, shared with the `ctx.fs` bindings.
#[derive(Clone)]
pub(crate) struct Capture {
    pub(crate) history: History,
    pub(crate) baseline: Baseline,
}

/// A call whose folders are being captured.
pub(crate) struct Started {
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) scans: bool,
    working: PathBuf,
    mark: u64,
    notes: Vec<String>,
    _lock: Option<FolderLock>,
}

impl Started {
    /// Mark `caller` as inside this capture, for the calls it makes.
    pub(crate) fn enclose(&self, caller: &mut Caller) {
        caller.capturing.extend(self.roots.iter().cloned());
        caller.capturing_scans |= self.scans;
    }
}

/// Whether a call can change files other than through `ctx.fs`: it holds a
/// `shell.*` grant, or it is a runner's tool call.
pub(crate) fn runs_programs(caller: &Caller, grants: &PermissionSet) -> bool {
    caller.runner.is_some() || grants.iter().any(|p| p.as_str().starts_with("shell."))
}

impl Capture {
    /// Start capturing `roots` for one call. With `lock`, wait a bounded time
    /// for each root and leave out those another call holds. With `scans`,
    /// scan the roots now, so changes made before the call are accepted as
    /// outside edits. Without it, outside edits to files with history are
    /// accepted directly.
    pub(crate) async fn begin(
        &self,
        working: PathBuf,
        roots: Vec<PathBuf>,
        scans: bool,
        lock: bool,
    ) -> Started {
        let mut notes = Vec::new();
        let (lock, roots) = if lock {
            let (lock, missed) = self.baseline.lock(roots, LOCK_WAIT).await;
            if !missed.is_empty() {
                notes.push(BUSY.to_owned());
            }
            let roots = lock.roots().to_vec();
            (Some(lock), roots)
        } else {
            (None, roots)
        };
        let mut kept = Vec::new();
        if scans {
            for root in roots {
                match self.scan(&root).await {
                    Ok(changes) => {
                        for change in changes {
                            if let Err(error) = self.history.acknowledge_outside(&change.path).await
                            {
                                tracing::debug!(path = %change.path.display(), %error, "an outside edit could not be accepted");
                            }
                        }
                        kept.push(root);
                    }
                    Err(error) => notes.push(scan_note(&root, &error)),
                }
            }
        } else {
            self.history.acknowledge_outside_in(&roots).await;
            kept = roots;
        }
        Started {
            roots: kept,
            scans,
            working,
            mark: self.history.mark(),
            notes,
            _lock: lock,
        }
    }

    /// Find what the call changed, record what file history does not have
    /// yet, and report it.
    pub(crate) async fn finish(
        &self,
        started: Started,
        actor: Actor,
    ) -> (Vec<FileChange>, Option<String>) {
        let mut reported = Vec::new();
        let mut notes = started.notes.clone();
        if !started.scans {
            for moved in self.history.moved_since(started.mark, &started.roots).await {
                reported.push(report(
                    &started.working,
                    &moved.path,
                    moved.before.as_ref(),
                    moved.after.as_ref(),
                    Some(moved.revision),
                ));
            }
        }
        for root in started.roots.iter().filter(|_| started.scans) {
            let changes = match self.scan(root).await {
                Ok(changes) => changes,
                Err(error) => {
                    notes.push(scan_note(root, &error));
                    continue;
                }
            };
            for change in changes {
                let digest = change.after.as_ref().map(|s| s.digest);
                // History already holds this state when `ctx.fs` wrote it,
                // or when an undo, redo or restore moved the file to it.
                let revision = match self.history.expected_revision(&change.path, digest).await {
                    Ok(Some(revision)) => Some(revision),
                    _ => self
                        .history
                        .record_external(
                            &change.path,
                            change.before.as_ref(),
                            change.after.as_ref(),
                            actor.clone(),
                        )
                        .await
                        .inspect_err(|error| {
                            tracing::debug!(path = %change.path.display(), %error, "a file change could not be recorded");
                        })
                        .ok(),
                };
                reported.push(report(
                    &started.working,
                    &change.path,
                    change.before.as_ref(),
                    change.after.as_ref(),
                    revision,
                ));
            }
        }
        drop(started);
        notes.dedup();
        let note = (!notes.is_empty()).then(|| notes.join(". "));
        (reported, note)
    }

    async fn scan(&self, root: &Path) -> Result<Vec<agentd_fs::ScanChange>, FsError> {
        let (baseline, root) = (self.baseline.clone(), root.to_path_buf());
        tokio::task::spawn_blocking(move || baseline.refresh(&root, ScanLimits::default()))
            .await
            .map_err(|e| FsError::Io(std::io::Error::other(e)))?
    }
}

fn report(
    working: &Path,
    path: &Path,
    before: Option<&Snapshot>,
    after: Option<&Snapshot>,
    revision: Option<usize>,
) -> FileChange {
    let shown = shown_path(working, path);
    let (diff, added, removed) = diff_for(before, after, &shown);
    FileChange {
        operation: match (before, after) {
            (None, _) => ChangeKind::Created,
            (_, None) => ChangeKind::Removed,
            _ => ChangeKind::Modified,
        },
        binary: diff.is_none() && !too_large(before) && !too_large(after),
        path: shown,
        revision,
        diff,
        added,
        removed,
    }
}

fn scan_note(root: &Path, error: &FsError) -> String {
    match error {
        FsError::TooManyFiles { .. } => TOO_LARGE.to_owned(),
        other => format!(
            "changes in `{}` were not tracked because it could not be scanned ({other})",
            root.display()
        ),
    }
}

/// The folders a call's file changes are captured in: the concrete roots of
/// its `fs.write` grants, clipped to `working`. A grant that covers `working`
/// gives `working` itself; a narrower one gives its own folder. Relative
/// grants resolve against `working`, as the `ctx.fs` bindings resolve them.
pub(crate) fn capture_roots(working: &Path, grants: &PermissionSet) -> Vec<PathBuf> {
    let working = canonical(working);
    let mut roots: Vec<PathBuf> = Vec::new();
    for permission in grants.iter() {
        let Some(("fs.write", spec)) = permission.as_str().split_once(':') else {
            continue;
        };
        if spec.contains("{cwd}") {
            continue;
        }
        // A relative grant with no folder before its first glob (`**`,
        // `*.md`) covers the working folder.
        let concrete = agentd_shell::policy::concrete_ancestor(spec);
        let grant = if concrete.as_os_str().is_empty() {
            working.clone()
        } else {
            canonical(&working.join(concrete))
        };
        let root = if working.starts_with(&grant) {
            working.clone()
        } else if grant.starts_with(&working) {
            grant
        } else {
            continue;
        };
        if roots.iter().any(|r| root.starts_with(r)) {
            continue;
        }
        roots.retain(|r| !r.starts_with(&root));
        roots.push(root);
    }
    roots
}

/// Replace `{cwd}` in path grants with the session folder. Without one, such
/// grants cover nothing, as in the `ctx.fs` bindings.
pub(crate) fn with_cwd(grants: &PermissionSet, cwd: Option<&str>) -> PermissionSet {
    let mut out = PermissionSet::default();
    for permission in grants.iter() {
        let text = permission.as_str();
        match (text.contains("{cwd}"), cwd) {
            (false, _) => out.insert(permission.clone()),
            (true, Some(cwd)) => out.insert(agentd_permissions::Permission::new(
                text.replace("{cwd}", cwd),
            )),
            (true, None) => {}
        }
    }
    out
}

/// `path` with every symlink resolved, as far as it exists. The `ctx.fs`
/// bindings resolve paths the same way, so both name a file alike.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(resolved) = existing.canonicalize() {
            let mut out = strip_verbatim(resolved);
            for part in missing.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Drop the `\\?\` prefix Windows adds to canonical paths.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_owned());
        }
    }
    path
}

/// `path` relative to the working folder when inside it, with `/`
/// separators, else in full.
fn shown_path(working: &Path, path: &Path) -> String {
    match path.strip_prefix(working) {
        Ok(relative) => relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Whether a side is over the size content is kept for. A side with no diff
/// that is not too large is reported as binary, as is text that is not UTF-8.
fn too_large(side: Option<&Snapshot>) -> bool {
    side.is_some_and(|s| s.size > ScanLimits::default().max_content_bytes)
}

/// A unified diff between two sides of a change, with the number of lines
/// added and removed. `None` when either side is binary, too large, or not
/// text.
pub(crate) fn diff_for(
    before: Option<&Snapshot>,
    after: Option<&Snapshot>,
    path: &str,
) -> (Option<String>, usize, usize) {
    let text = |side: Option<&Snapshot>| -> Option<String> {
        match side {
            None => Some(String::new()),
            Some(snapshot) => String::from_utf8(snapshot.content.clone()?).ok(),
        }
    };
    let (Some(old), Some(new)) = (text(before), text(after)) else {
        return (None, 0, 0);
    };
    let diff = agentd_fs::create_diff(&old, &new, path, path, 3);
    let (mut added, mut removed) = (0, 0);
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    let lines: Vec<&str> = diff.lines().collect();
    let diff = if lines.len() > MAX_DIFF_LINES {
        let mut cut = lines[..MAX_DIFF_LINES].join("\n");
        cut.push_str(&format!(
            "\n... {} more lines\n",
            lines.len() - MAX_DIFF_LINES
        ));
        cut
    } else {
        diff
    };
    (Some(diff), added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: &str) -> Snapshot {
        Snapshot {
            size: text.len() as u64,
            digest: [0; 32],
            content: Some(text.as_bytes().to_vec()),
        }
    }

    #[test]
    fn diffs_count_lines_and_are_cut_when_long() {
        let (diff, added, removed) = diff_for(
            Some(&snapshot("a\nb\n")),
            Some(&snapshot("a\nc\n")),
            "f.txt",
        );
        let diff = diff.unwrap();
        assert!(diff.contains("-b") && diff.contains("+c"), "{diff}");
        assert_eq!((added, removed), (1, 1));

        let long: String = (0..1000).map(|i| format!("{i}\n")).collect();
        let (diff, added, _) = diff_for(None, Some(&snapshot(&long)), "long.txt");
        let diff = diff.unwrap();
        assert_eq!(added, 1000);
        assert_eq!(diff.lines().count(), MAX_DIFF_LINES + 1);
        assert!(diff.ends_with("more lines\n"), "{diff}");
    }

    #[test]
    fn binary_sides_have_no_diff() {
        let binary = Snapshot {
            size: 3,
            digest: [1; 32],
            content: None,
        };
        let (diff, added, removed) = diff_for(None, Some(&binary), "b.bin");
        assert_eq!((diff, added, removed), (None, 0, 0));
        assert!(
            report(
                Path::new("/w"),
                Path::new("/w/b.bin"),
                None,
                Some(&binary),
                None
            )
            .binary
        );
    }

    #[test]
    fn text_that_is_not_utf8_is_reported_as_binary() {
        let latin1 = Snapshot {
            size: 3,
            digest: [2; 32],
            content: Some(vec![b'c', 0xe9, b'\n']),
        };
        let change = report(
            Path::new("/w"),
            Path::new("/w/l.txt"),
            None,
            Some(&latin1),
            Some(1),
        );
        assert!(change.diff.is_none());
        assert!(change.binary);
        assert_eq!(change.path, "l.txt");
    }

    #[test]
    fn a_relative_glob_with_no_folder_covers_the_working_folder() {
        let dir = tempfile::tempdir().unwrap();
        let working = dir.path().canonicalize().unwrap();
        for spec in ["fs.write:**", "fs.write:*.md"] {
            let grants = PermissionSet::from_iter([spec]);
            assert_eq!(
                capture_roots(&working, &grants),
                vec![working.clone()],
                "{spec}"
            );
        }
    }

    #[test]
    fn roots_are_write_grants_clipped_to_the_working_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let working = root.join("project");
        std::fs::create_dir_all(working.join("src")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        let at = |spec: &str| format!("fs.write:{}", root.join(spec).display());

        let broad = PermissionSet::from_iter([at("**")]);
        assert_eq!(capture_roots(&working, &broad), vec![working.clone()]);

        let narrow = PermissionSet::from_iter([at("project/src/**")]);
        assert_eq!(capture_roots(&working, &narrow), vec![working.join("src")]);

        let elsewhere = PermissionSet::from_iter([at("other/**")]);
        assert!(capture_roots(&working, &elsewhere).is_empty());

        let relative = PermissionSet::from_iter(["fs.write:src/**"]);
        assert_eq!(
            capture_roots(&working, &relative),
            vec![working.join("src")]
        );

        let reads = PermissionSet::from_iter([format!("fs.read:{}/**", root.display())]);
        assert!(capture_roots(&working, &reads).is_empty());

        let both = PermissionSet::from_iter([at("project/src/**"), at("**")]);
        assert_eq!(capture_roots(&working, &both), vec![working.clone()]);
    }
}

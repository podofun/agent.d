//! Capture every file an action call changes, however it changed it.
//!
//! Before a call, the folders it may write are scanned against a shared
//! baseline. Anything that changed since the last scan was changed outside
//! agent.d, so file history accepts it without reporting it. After the call
//! a second scan finds what the call changed. Changes `ctx.fs` already
//! recorded keep their revision; anything else is recorded as an external
//! revision. Both are reported with a unified diff.
//!
//! Calls that write the same folder take turns through a folder lock held
//! from the first scan to the second, so each call's changes are its own.
//! A call nested inside another (an action that starts a runner whose tools
//! write the same folder) does not wait for that lock: the enclosing call
//! names the folders it holds on the caller, the nested call skips them,
//! and the enclosing call's second scan reports the nested changes.

use std::path::{Path, PathBuf};

use agentd_ai::{ChangeKind, FileChange};
use agentd_fs::{Actor, Baseline, FolderLock, FsError, History, ScanLimits, Snapshot};
use agentd_permissions::PermissionSet;

/// A diff longer than this many lines is cut, with a final line saying how
/// many were left out.
const MAX_DIFF_LINES: usize = 400;

/// The note a call's trace carries when a folder was too large to scan.
const TOO_LARGE: &str = "the folder was too large to track changes in";

/// File history and the folder baseline, shared with the `ctx.fs` bindings.
#[derive(Clone)]
pub(crate) struct Capture {
    pub(crate) history: History,
    pub(crate) baseline: Baseline,
}

/// A call whose folders were scanned before it ran.
pub(crate) struct Started {
    pub(crate) roots: Vec<PathBuf>,
    working: PathBuf,
    mark: u64,
    note: Option<String>,
    _lock: FolderLock,
}

impl Capture {
    /// Hold `roots` and scan them. Changes found now were made outside
    /// agent.d: file history accepts them so the call can write those files.
    pub(crate) async fn begin(&self, working: PathBuf, roots: Vec<PathBuf>) -> Started {
        let lock = self.baseline.lock(roots.clone()).await;
        let mut kept = Vec::new();
        let mut note = None;
        for root in roots {
            match self.scan(&root).await {
                Ok(changes) => {
                    for change in changes {
                        if let Err(error) = self.history.acknowledge_outside(&change.path).await {
                            tracing::debug!(path = %change.path.display(), %error, "an outside edit could not be accepted");
                        }
                    }
                    kept.push(root);
                }
                Err(error) => note = Some(scan_note(&root, &error)),
            }
        }
        Started {
            roots: kept,
            working,
            mark: self.history.mark(),
            note,
            _lock: lock,
        }
    }

    /// Scan again after the call, record what it changed, and report it.
    pub(crate) async fn finish(
        &self,
        started: Started,
        actor: Actor,
    ) -> (Vec<FileChange>, Option<String>) {
        let mut reported = Vec::new();
        let mut note = started.note.clone();
        for root in &started.roots {
            let changes = match self.scan(root).await {
                Ok(changes) => changes,
                Err(error) => {
                    note = Some(scan_note(root, &error));
                    continue;
                }
            };
            for change in changes {
                let digest = change.after.as_ref().map(|s| s.digest);
                let revision = match self
                    .history
                    .recorded_since(&change.path, started.mark, digest)
                    .await
                {
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
                let path = shown_path(&started.working, &change.path);
                let (diff, added, removed) =
                    diff_for(change.before.as_ref(), change.after.as_ref(), &path);
                let binary = diff.is_none() && is_binary(&change.before, &change.after);
                reported.push(FileChange {
                    operation: match (&change.before, &change.after) {
                        (None, _) => ChangeKind::Created,
                        (_, None) => ChangeKind::Removed,
                        _ => ChangeKind::Modified,
                    },
                    path,
                    revision,
                    diff,
                    binary,
                    added,
                    removed,
                });
            }
        }
        drop(started);
        (reported, note)
    }

    async fn scan(&self, root: &Path) -> Result<Vec<agentd_fs::ScanChange>, FsError> {
        let (baseline, root) = (self.baseline.clone(), root.to_path_buf());
        tokio::task::spawn_blocking(move || baseline.refresh(&root, ScanLimits::default()))
            .await
            .map_err(|e| FsError::Io(std::io::Error::other(e)))?
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
        let concrete = agentd_shell::policy::concrete_ancestor(spec);
        if concrete.as_os_str().is_empty() {
            continue;
        }
        let grant = canonical(&working.join(concrete));
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

/// A side that exists but kept no content was binary or too large.
fn is_binary(before: &Option<Snapshot>, after: &Option<Snapshot>) -> bool {
    let opaque = |side: &Option<Snapshot>| {
        side.as_ref().is_some_and(|s| {
            s.content.is_none() && s.size <= ScanLimits::default().max_content_bytes
        })
    };
    opaque(before) || opaque(after)
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
        assert!(is_binary(&None, &Some(binary)));
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

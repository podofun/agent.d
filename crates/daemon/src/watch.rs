//! Dev hot-reload watcher (`agentd --watch`).
//!
//! Watches the *used* file set — `init.lua`, its `import()` targets, loaded
//! skill sources, and `grants.toml` — and rebuilds the runtime in place on
//! change. The rebuild swaps a fresh [`Executor`] behind the API's `ArcSwap`,
//! so live WebSocket connections survive; in-flight calls drain on the old
//! runtime, which is then torn down (see [`crate::runtime::teardown`]).
//!
//! Watching strategy: watch the *parent directory* of each used file
//! (non-recursive) and filter incoming events down to the exact used set.
//! Editors save via temp-write + atomic rename, which breaks inode-level
//! watches; dir-watch + path filter survives it. Filtering also means writes
//! the daemon itself makes under the project dir (e.g. `.luals/`) never
//! retrigger a reload, because those paths are not in the used set.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd_executor::Executor;
use agentd_types::Registry;
use notify::{RecursiveMode, Watcher};

use crate::config::Config;
use crate::runtime::{BuiltRuntime, Shared, build_runtime, teardown};

const DEBOUNCE: Duration = Duration::from_millis(200);

/// Whether an event represents a content change worth reloading for. Pure
/// `Access` events (open/read/close-read) are excluded: a reload reads every
/// watched file, so reacting to reads would self-trigger forever.
fn is_mutation(kind: &notify::EventKind) -> bool {
    use notify::EventKind;
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// Start the watcher on the shared tokio runtime. Takes ownership of the
/// initial runtime so it can tear it down when the first reload supersedes it.
pub fn spawn(
    cfg: Config,
    shared: Shared,
    executor: Arc<arc_swap::ArcSwap<Executor>>,
    initial: BuiltRuntime,
) {
    let handle = shared.async_handle.clone();
    handle.spawn(async move {
        if let Err(e) = watch_loop(cfg, shared, executor, initial).await {
            tracing::error!(error = %e, "watch loop exited");
        }
    });
}

async fn watch_loop(
    cfg: Config,
    shared: Shared,
    executor: Arc<arc_swap::ArcSwap<Executor>>,
    mut current: BuiltRuntime,
) -> anyhow::Result<()> {
    // The set of files whose changes should trigger a reload, shared with the
    // notify callback (which runs on its own thread). Updated after each reload.
    let used: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(canon_set(&current.used_paths)));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let used_for_cb = used.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        // React only to content mutations. A reload itself *reads* every used
        // file (init, imports, skills, grants), which fires `Access`/open events
        // on those very paths — reacting to those would make each reload trigger
        // the next one in an unbounded loop.
        if !is_mutation(&event.kind) {
            return;
        }
        let set = used_for_cb.lock().unwrap();
        for p in &event.paths {
            let hit = canon(p);
            if set.contains(&hit) {
                tracing::debug!(path = %hit.display(), kind = ?event.kind, "watch trigger");
                let _ = tx.send(());
                break;
            }
        }
    })?;

    let mut watched: HashSet<PathBuf> = HashSet::new();
    arm(&mut watcher, &mut watched, &current.used_paths);
    tracing::info!(
        files = current.used_paths.len(),
        dirs = watched.len(),
        "watch armed"
    );
    // Regenerate per-project types for the initial runtime too, so `.luals/`
    // exists as soon as the watcher comes up.
    regen_types(&cfg, &current);

    while rx.recv().await.is_some() {
        // Debounce: coalesce a burst of saves into a single reload.
        tokio::time::sleep(DEBOUNCE).await;
        while rx.try_recv().is_ok() {}

        let started = Instant::now();
        match build_runtime(&cfg, &shared) {
            Ok(new) => {
                // Re-arm to the new used set (a new import widens it) and publish
                // it to the callback before swapping.
                *used.lock().unwrap() = canon_set(&new.used_paths);
                arm(&mut watcher, &mut watched, &new.used_paths);

                executor.store(new.executor.clone());
                regen_types(&cfg, &new);

                let counts = (
                    new.host.list().len(),
                    new.host.runners().len(),
                    new.host.services().len(),
                    new.host.skills().len(),
                );
                let old = std::mem::replace(&mut current, new);
                teardown(old);

                let (a, r, s, k) = counts;
                println!(
                    "  reloaded in {} ms — {a} actions, {r} runners, {s} services, {k} skills",
                    started.elapsed().as_millis()
                );
            }
            Err(e) => {
                // Keep the old runtime serving; surface the error and wait for
                // the next save.
                tracing::error!(error = %e, "reload failed; keeping previous runtime");
                println!("  reload failed: {e}");
            }
        }
    }
    Ok(())
}

/// Point the watcher at the parent directory of every used file. Unwatches dirs
/// that are no longer relevant and watches newly-relevant ones.
fn arm(watcher: &mut notify::RecommendedWatcher, watched: &mut HashSet<PathBuf>, used: &[PathBuf]) {
    let want: HashSet<PathBuf> = used.iter().filter_map(|p| parent_dir(p)).collect();
    for dir in watched.iter() {
        if !want.contains(dir) {
            let _ = watcher.unwatch(dir);
        }
    }
    for dir in &want {
        if !watched.contains(dir)
            && let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive)
        {
            tracing::warn!(dir = %dir.display(), error = %e, "watch dir failed");
        }
    }
    *watched = want;
}

/// Existing parent directory of `p`, canonicalized.
fn parent_dir(p: &Path) -> Option<PathBuf> {
    let parent = p.parent()?;
    let dir = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    dir.canonicalize().ok()
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn canon_set(paths: &[PathBuf]) -> HashSet<PathBuf> {
    paths.iter().map(|p| canon(p)).collect()
}

/// Regenerate the project's `.luals/` type stubs from the live registry. A
/// failure here is non-fatal to the reload.
fn regen_types(cfg: &Config, built: &BuiltRuntime) {
    if let Some(dir) = cfg.init_file.parent() {
        let actions = built.host.list();
        let runners = built.host.runners().names();
        let skills = built.host.skills().names();
        if let Err(e) = agentd_luals::write_project(dir, &actions, &runners, &skills) {
            tracing::warn!(error = %e, "luals typegen failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_mutation;
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};

    #[test]
    fn access_events_are_ignored() {
        // A reload reads every watched file; those reads must NOT re-trigger it.
        assert!(!is_mutation(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!is_mutation(&EventKind::Access(AccessKind::Read)));
        assert!(!is_mutation(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!is_mutation(&EventKind::Any));
    }

    #[test]
    fn content_changes_trigger() {
        assert!(is_mutation(&EventKind::Modify(ModifyKind::Any)));
        assert!(is_mutation(&EventKind::Create(CreateKind::File)));
        assert!(is_mutation(&EventKind::Remove(RemoveKind::File)));
    }
}

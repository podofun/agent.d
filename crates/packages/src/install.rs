use crate::index::IndexEntry;
use std::path::Path;
use std::process::Command;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("git {0} failed: {1}")]
    Git(String, String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<String, InstallError> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(InstallError::Git(
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Clone `url` (optionally at `reference`) into `<dest_root>/<name>`, resolve
/// the checked-out commit, return the index entry to record.
pub fn install(
    url: &str,
    reference: Option<&str>,
    dest_root: &Path,
    name: &str,
) -> Result<IndexEntry, InstallError> {
    let dest = dest_root.join(name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)?;
    }
    std::fs::create_dir_all(dest_root)?;
    git(&["clone", "-q", url, dest.to_str().unwrap()], None)?;
    if let Some(r) = reference {
        git(&["checkout", "-q", r], Some(&dest))?;
    }
    let commit = git(&["rev-parse", "HEAD"], Some(&dest))?;
    Ok(IndexEntry {
        name: name.into(),
        url: url.into(),
        r#ref: reference.unwrap_or("HEAD").into(),
        commit,
    })
}

/// `git fetch` then re-pin to the ref's tip. Returns the new commit.
pub fn update(entry: &IndexEntry, dir: &Path) -> Result<String, InstallError> {
    git(&["fetch", "-q", "origin"], Some(dir))?;
    let target = if entry.r#ref == "HEAD" {
        "origin/HEAD".to_string()
    } else {
        format!("origin/{}", entry.r#ref)
    };
    // Fall back to the bare ref (tag) if origin/<ref> doesn't resolve.
    let commit = git(&["rev-parse", &target], Some(dir))
        .or_else(|_| git(&["rev-parse", &entry.r#ref], Some(dir)))?;
    git(&["checkout", "-q", &commit], Some(dir))?;
    Ok(commit)
}

/// True if upstream has commits beyond the pinned one (an update is available).
pub fn update_check(entry: &IndexEntry, dir: &Path) -> Result<bool, InstallError> {
    git(&["fetch", "-q", "origin"], Some(dir))?;
    let target = if entry.r#ref == "HEAD" {
        "origin/HEAD".to_string()
    } else {
        format!("origin/{}", entry.r#ref)
    };
    let remote = match git(&["rev-parse", &target], Some(dir)) {
        Ok(c) => c,
        Err(_) => return Ok(false), // ref no longer resolves remotely; don't nag
    };
    Ok(remote != entry.commit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    // Build a throwaway git repo on disk with a package.toml, return its path.
    fn make_repo(dir: &std::path::Path) -> String {
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(
            dir.join("package.toml"),
            "[package]\nname = \"fix\"\nentry = \"main.lua\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "-- pkg").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "init"]);
        dir.display().to_string()
    }

    #[test]
    fn install_clones_and_pins() {
        let src = tempdir().unwrap();
        let url = make_repo(src.path());
        let dest_root = tempdir().unwrap();

        let entry = install(&url, None, dest_root.path(), "fix").unwrap();
        assert_eq!(entry.name, "fix");
        assert_eq!(entry.commit.len(), 40); // full sha
        assert!(dest_root.path().join("fix/package.toml").exists());
    }

    #[test]
    fn update_check_reports_no_update_when_current() {
        let src = tempdir().unwrap();
        let url = make_repo(src.path());
        let dest_root = tempdir().unwrap();
        let entry = install(&url, None, dest_root.path(), "fix").unwrap();
        let behind = update_check(&entry, &dest_root.path().join("fix")).unwrap();
        assert!(
            !behind,
            "freshly installed repo should not report an update"
        );
    }
}

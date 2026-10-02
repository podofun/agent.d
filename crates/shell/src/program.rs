//! Find the executable a program name or path refers to, the way the shell
//! would run it.
//!
//! Both sides of a `shell.exec` permission are compared through this: the
//! program a script asks for, and a program named in a grant or denial. So
//! `shell.exec:python3` and a request for `/usr/bin/python3` meet at the same
//! resolved file, symlinks included.

use std::path::{Path, PathBuf};

/// Resolve `program` to the canonical path of the executable it names.
///
/// A path (anything containing a separator) is taken relative to `cwd` when it
/// is not absolute. A bare name is looked up on the daemon's `PATH`. On
/// Windows a name or path without the `.exe` suffix also matches the `.exe`
/// file. Returns `None` when nothing executable is found.
pub fn resolve_program(program: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        let full = match cwd {
            Some(dir) if !path.is_absolute() => dir.join(path),
            _ => path.to_path_buf(),
        };
        return candidates(&full).into_iter().find_map(executable);
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|dir| candidates(&dir.join(program)))
        .find_map(executable)
}

/// The files a path may name: itself, and on Windows also with `.exe`.
fn candidates(path: &Path) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut with_exe = path.as_os_str().to_owned();
        with_exe.push(".exe");
        vec![path.to_path_buf(), PathBuf::from(with_exe)]
    }
    #[cfg(not(windows))]
    {
        vec![path.to_path_buf()]
    }
}

/// The canonical path of `path` when it is a file this process could run.
fn executable(path: PathBuf) -> Option<PathBuf> {
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    let canonical = path.canonicalize().ok()?;
    #[cfg(windows)]
    {
        let text = canonical.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return Some(PathBuf::from(rest));
        }
    }
    Some(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_tool(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_path_resolves_through_symlinks_and_relative_to_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let real = make_tool(&root, "realtool");
        std::os::unix::fs::symlink(&real, root.join("aliastool")).unwrap();
        assert_eq!(
            resolve_program(&root.join("aliastool").to_string_lossy(), None),
            Some(real.clone())
        );
        assert_eq!(resolve_program("./aliastool", Some(&root)), Some(real));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_executable_is_not_a_program() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(resolve_program(&file.to_string_lossy(), None), None);
    }

    #[test]
    fn a_name_resolves_on_the_path() {
        #[cfg(unix)]
        let name = "sh";
        #[cfg(windows)]
        let name = "cmd";
        let found = resolve_program(name, None).expect("found on PATH");
        assert!(found.is_absolute(), "{}", found.display());
    }

    #[test]
    fn nothing_is_found_for_an_unknown_program() {
        assert_eq!(resolve_program("agentd-no-such-program-xyz", None), None);
        assert_eq!(resolve_program("", None), None);
    }
}

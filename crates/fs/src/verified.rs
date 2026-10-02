//! Open a file and confirm the handle is exactly the file whose path was
//! checked.
//!
//! A permission check and the open that follows are two separate steps, so a
//! path component swapped for a symlink in between would let the open reach a
//! different file than the one that was allowed. Asking the operating system
//! which file the open handle refers to closes that gap: if it is not the
//! checked path, the handle is dropped before anything is read from it.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::FsError;

/// The path the operating system reports for an open file.
pub fn real_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes, including the
        // terminating NUL, into a buffer of that size.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..len])))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW,
        };
        let handle = HANDLE(file.as_raw_handle());
        let mut buf = vec![0u16; 512];
        loop {
            // SAFETY: the handle is valid for the borrow of `file`, and the
            // call writes at most `buf.len()` UTF-16 units.
            let n = unsafe { GetFinalPathNameByHandleW(handle, &mut buf, FILE_NAME_NORMALIZED) }
                as usize;
            if n == 0 {
                return Err(std::io::Error::last_os_error());
            }
            if n < buf.len() {
                let path = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
                return Ok(strip_verbatim(path));
            }
            buf.resize(n + 1, 0);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = file;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "this platform cannot report which file an open handle refers to",
        ))
    }
}

/// `\\?\C:\x` becomes `C:\x` and `\\?\UNC\server\share` becomes
/// `\\server\share`, the forms `std::fs::canonicalize` callers compare with.
#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

/// Whether two absolute paths name the same file on this platform. macOS and
/// Windows file systems ignore letter case by default, and Windows paths may
/// carry the `\\?\` prefix `std::fs::canonicalize` adds.
fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    let (a, b) = (
        strip_verbatim(a.to_path_buf()),
        strip_verbatim(b.to_path_buf()),
    );
    if cfg!(any(target_os = "macos", windows)) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Open `path` for reading, but only if the handle is exactly the file at
/// `path`. `path` is expected to be already resolved (absolute, no symlinks),
/// as the permission check saw it; anything else means it changed in between.
pub fn open_verified(path: &Path) -> Result<std::fs::File, FsError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(FsError::NotFound(path.into()));
        }
        Err(e) => return Err(e.into()),
    };
    let actual = real_path(&file)?;
    if !same_path(&actual, path) {
        return Err(FsError::Changed {
            path: path.into(),
            actual,
        });
    }
    Ok(file)
}

/// Read the whole file at `path` through a verified handle (see
/// [`open_verified`]).
pub async fn read_bytes_verified(path: &Path) -> Result<Vec<u8>, FsError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut bytes = Vec::new();
        open_verified(&path)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await
    .map_err(|e| FsError::Io(std::io::Error::other(e)))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_temp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        #[cfg(windows)]
        let root = strip_verbatim(root);
        (dir, root)
    }

    #[test]
    fn the_os_reports_the_path_a_handle_was_opened_with() {
        let (_dir, root) = canonical_temp();
        let file = root.join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        let opened = std::fs::File::open(&file).unwrap();
        assert!(same_path(&real_path(&opened).unwrap(), &file));
    }

    #[tokio::test]
    async fn a_verified_read_returns_the_contents() {
        let (_dir, root) = canonical_temp();
        let file = root.join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        assert_eq!(read_bytes_verified(&file).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn a_missing_file_is_reported_as_not_found() {
        let (_dir, root) = canonical_temp();
        assert!(matches!(
            read_bytes_verified(&root.join("missing")).await,
            Err(FsError::NotFound(_))
        ));
    }

    /// A path that leads somewhere else by the time it is opened (here a
    /// symlink standing where the checked file was) is refused, and nothing is
    /// read from the file it leads to.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_path_that_now_leads_elsewhere_is_refused() {
        let (_dir, root) = canonical_temp();
        let secret = root.join("secret.txt");
        std::fs::write(&secret, b"secret").unwrap();
        let checked = root.join("allowed.txt");
        std::os::unix::fs::symlink(&secret, &checked).unwrap();
        match read_bytes_verified(&checked).await {
            Err(FsError::Changed { path, actual }) => {
                assert_eq!(path, checked);
                assert!(same_path(&actual, &secret), "{}", actual.display());
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }
}

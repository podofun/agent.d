//! What a sandboxed child can reach, so a caller can tell whether a path it
//! must keep away from the child is within that reach.
//!
//! The sandbox can only allow: it cannot carve one file out of a folder it
//! opens, against a child that can rename folders or create links. So reach
//! is judged loosely. A path and a folder the sandbox opens overlap when
//! either contains the other, by name, through a bind mount (Linux), or
//! through a hard link to the same file.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::policy::{SandboxPolicy, WRITE_SCRATCH};

/// How a child could reach a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlap {
    /// The sandbox is off (`shell.unrestricted`), so everything is reachable.
    Unrestricted,
    /// The sandbox opens this folder, and it and the path contain one another.
    Folder(PathBuf),
    /// The sandbox opens `folder`, and the mount at `mount` leads to the path.
    Mount { folder: PathBuf, mount: PathBuf },
    /// The path is a file with more than one hard link, and the sandbox opens
    /// this folder on the same device, where another link may live.
    HardLink(PathBuf),
}

impl fmt::Display for Overlap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Overlap::Unrestricted => write!(f, "it runs unrestricted (`shell.unrestricted`)"),
            Overlap::Folder(folder) => {
                write!(
                    f,
                    "its sandbox opens `{}`, which overlaps it",
                    folder.display()
                )
            }
            Overlap::Mount { folder, mount } => write!(
                f,
                "its sandbox opens `{}`, and the mount at `{}` leads to it",
                folder.display(),
                mount.display()
            ),
            Overlap::HardLink(folder) => write!(
                f,
                "it is a file with more than one hard link, and its sandbox opens `{}` on the same device",
                folder.display()
            ),
        }
    }
}

/// Every folder a child under one policy can read and write: the policy's
/// grants plus the fixed sets the platform's sandbox always opens.
#[derive(Debug, Clone)]
pub struct Reach {
    unrestricted: bool,
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
}

impl Reach {
    pub fn of(policy: &SandboxPolicy) -> Self {
        let read = fixed_reads()
            .into_iter()
            .chain(policy.read_paths.iter().cloned())
            .chain(policy.write_paths.iter().cloned())
            .map(|p| real(&p))
            .collect();
        let write = policy
            .write_paths
            .iter()
            .cloned()
            .chain(WRITE_SCRATCH.iter().map(PathBuf::from))
            .map(|p| real(&p))
            .collect();
        Self {
            unrestricted: policy.unrestricted,
            read,
            write,
        }
    }

    /// How a child could read `path` (or write it, with `write`), if it can.
    pub fn overlap(&self, path: &Path, write: bool) -> Option<Overlap> {
        if self.unrestricted {
            return Some(Overlap::Unrestricted);
        }
        let folders = if write { &self.write } else { &self.read };
        let target = real(path);
        if let Some(folder) = folders.iter().find(|f| nested(f, &target)) {
            return Some(Overlap::Folder(folder.clone()));
        }
        let mounts = mounts::table();
        mounts::overlap(&mounts, folders, &target).or_else(|| hard_link(folders, &target, &mounts))
    }
}

/// The folders every sandboxed child may read, whatever its grants.
fn fixed_reads() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(unix)]
    out.extend(crate::policy::READ_BASELINE.iter().map(PathBuf::from));
    #[cfg(target_os = "macos")]
    out.extend(crate::policy::MACOS_READ_EXTRA.iter().map(PathBuf::from));
    out.extend(crate::policy::user_read_baseline());
    out
}

/// `path` with every symlink in its existing part resolved, the way the
/// kernel sees it. A missing tail is kept as written.
fn real(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail = Vec::new();
    loop {
        if let Ok(found) = existing.canonicalize() {
            let mut out = strip_verbatim(found);
            out.extend(tail.iter().rev());
            return out;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_owned());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    match path.to_string_lossy().strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

/// Whether either path contains the other, without regard to letter case
/// where the platform's file names ignore it.
fn nested(a: &Path, b: &Path) -> bool {
    let (a, b) = (fold(a), fold(b));
    a.starts_with(&b) || b.starts_with(&a)
}

#[cfg(any(target_os = "macos", windows))]
fn fold(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn fold(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// A file with several hard links may have one inside any folder on its
/// device, so such a folder, or a mount on that device beneath it, reaches it.
#[cfg(unix)]
fn hard_link(folders: &[PathBuf], target: &Path, mounts: &[mounts::Mount]) -> Option<Overlap> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(target).ok()?;
    if !meta.is_file() || meta.nlink() < 2 {
        return None;
    }
    let on_device = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.dev() == meta.dev());
    folders
        .iter()
        .find(|folder| {
            on_device(folder)
                || mounts
                    .iter()
                    .any(|m| m.point.starts_with(folder) && on_device(&m.point))
        })
        .map(|folder| Overlap::HardLink(folder.clone()))
}

#[cfg(not(unix))]
fn hard_link(_folders: &[PathBuf], _target: &Path, _mounts: &[mounts::Mount]) -> Option<Overlap> {
    None
}

/// Bind mounts show the same files under another name. Linux lists every
/// mount with its device and the folder of that device it shows, so two
/// paths are compared as (device, path within the device).
mod mounts {
    use std::path::{Path, PathBuf};

    use super::{Overlap, nested};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Mount {
        /// The device, as `major:minor`.
        pub dev: String,
        /// The folder of the device this mount shows.
        pub root: PathBuf,
        /// Where it is mounted.
        pub point: PathBuf,
    }

    #[cfg(target_os = "linux")]
    pub fn table() -> Vec<Mount> {
        std::fs::read_to_string("/proc/self/mountinfo")
            .map(|text| parse(&text))
            .unwrap_or_default()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn table() -> Vec<Mount> {
        Vec::new()
    }

    /// Parse `/proc/self/mountinfo`: `id parent major:minor root point ...`.
    #[cfg(any(target_os = "linux", test))]
    pub fn parse(text: &str) -> Vec<Mount> {
        text.lines()
            .filter_map(|line| {
                let mut fields = line.split(' ');
                let _id = fields.next()?;
                let _parent = fields.next()?;
                let dev = fields.next()?.to_string();
                let root = PathBuf::from(unescape(fields.next()?));
                let point = PathBuf::from(unescape(fields.next()?));
                Some(Mount { dev, root, point })
            })
            .collect()
    }

    /// Undo the octal escapes (`\040` for a space) mountinfo writes.
    #[cfg(any(target_os = "linux", test))]
    fn unescape(field: &str) -> String {
        let bytes = field.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let octal = bytes
                .get(i + 1..i + 4)
                .filter(|d| d.iter().all(|b| (b'0'..=b'7').contains(b)));
            if bytes[i] == b'\\'
                && let Some(digits) = octal
            {
                let code = digits.iter().fold(0u32, |n, d| n * 8 + u32::from(d - b'0'));
                out.push(code as u8);
                i += 4;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// The device holding `path` and the path within it, through the mount
    /// whose point is the longest prefix. Of mounts on the same point, the
    /// last one listed is on top.
    fn locate<'a>(table: &'a [Mount], path: &Path) -> Option<(&'a Mount, PathBuf)> {
        let mount = table
            .iter()
            .filter(|m| path.starts_with(&m.point))
            .max_by_key(|m| m.point.components().count())?;
        let rest = path.strip_prefix(&mount.point).ok()?;
        Some((mount, mount.root.join(rest)))
    }

    /// A folder reaches `target` when, on the same device, the folder's own
    /// place or a mount beneath it and the target's place contain one another.
    pub fn overlap(table: &[Mount], folders: &[PathBuf], target: &Path) -> Option<Overlap> {
        let (target_mount, target_rel) = locate(table, target)?;
        for folder in folders {
            if let Some((mount, rel)) = locate(table, folder)
                && mount.dev == target_mount.dev
                && nested(&rel, &target_rel)
            {
                return Some(Overlap::Mount {
                    folder: folder.clone(),
                    mount: mount.point.clone(),
                });
            }
            if let Some(mount) = table.iter().find(|m| {
                m.point.starts_with(folder)
                    && m.point != *folder
                    && m.dev == target_mount.dev
                    && nested(&m.root, &target_rel)
            }) {
                return Some(Overlap::Mount {
                    folder: folder.clone(),
                    mount: mount.point.clone(),
                });
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const TABLE: &str = "\
22 1 8:1 / / rw shared:1 - ext4 /dev/sda1 rw
30 22 8:2 / /home rw shared:2 - ext4 /dev/sda2 rw
40 30 8:2 /u/secret /srv/view rw - ext4 /dev/sda2 rw
41 22 8:2 /u/secret/keys /opt/app/keys rw - ext4 /dev/sda2 rw
42 22 0:5 / /mnt/with\\040space rw - tmpfs tmpfs rw
";

        fn p(s: &str) -> PathBuf {
            PathBuf::from(s)
        }

        #[test]
        fn mountinfo_is_parsed_with_escapes() {
            let table = parse(TABLE);
            assert_eq!(table.len(), 5);
            assert_eq!(table[2].root, p("/u/secret"));
            assert_eq!(table[4].point, p("/mnt/with space"));
        }

        #[test]
        fn a_folder_that_is_a_bind_of_the_denied_folder_reaches_it() {
            let table = parse(TABLE);
            let got = overlap(&table, &[p("/srv/view")], &p("/home/u/secret/.env"));
            assert_eq!(
                got,
                Some(Overlap::Mount {
                    folder: p("/srv/view"),
                    mount: p("/srv/view")
                })
            );
        }

        #[test]
        fn a_bind_beneath_a_folder_reaches_what_it_shows() {
            let table = parse(TABLE);
            let got = overlap(&table, &[p("/opt")], &p("/home/u/secret"));
            assert_eq!(
                got,
                Some(Overlap::Mount {
                    folder: p("/opt"),
                    mount: p("/opt/app/keys")
                })
            );
        }

        #[test]
        fn unrelated_folders_on_other_devices_or_places_do_not() {
            let table = parse(TABLE);
            assert_eq!(
                overlap(&table, &[p("/usr"), p("/mnt")], &p("/home/u/secret/.env")),
                None
            );
            assert_eq!(
                overlap(&table, &[p("/home/u/proj")], &p("/home/u/secret/.env")),
                None
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(read: &[&Path], write: &[&Path]) -> SandboxPolicy {
        SandboxPolicy {
            read_paths: read.iter().map(|p| p.to_path_buf()).collect(),
            write_paths: write.iter().map(|p| p.to_path_buf()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn a_granted_folder_overlaps_what_it_contains_and_what_contains_it() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("proj");
        std::fs::create_dir(&proj).unwrap();
        let reach = Reach::of(&policy(&[&proj], &[]));
        assert!(matches!(
            reach.overlap(&proj.join(".env"), false),
            Some(Overlap::Folder(_))
        ));
        assert!(matches!(
            reach.overlap(dir.path(), false),
            Some(Overlap::Folder(_))
        ));
        assert_eq!(reach.overlap(&dir.path().join("other/.env"), false), None);
    }

    #[test]
    fn reads_and_writes_are_judged_apart() {
        let dir = tempfile::tempdir().unwrap();
        let reach = Reach::of(&policy(&[dir.path()], &[]));
        assert!(reach.overlap(&dir.path().join("f"), false).is_some());
        assert_eq!(reach.overlap(&dir.path().join("f"), true), None);
        let reach = Reach::of(&policy(&[], &[dir.path()]));
        assert!(reach.overlap(&dir.path().join("f"), true).is_some());
        assert!(reach.overlap(&dir.path().join("f"), false).is_some());
    }

    #[test]
    fn unrestricted_reaches_everything() {
        let reach = Reach::of(&SandboxPolicy {
            unrestricted: true,
            ..Default::default()
        });
        assert_eq!(
            reach.overlap(Path::new("/anywhere"), true),
            Some(Overlap::Unrestricted)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_granted_through_a_symlink_overlaps_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::create_dir(&secret).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&secret, &link).unwrap();
        let reach = Reach::of(&policy(&[&link], &[]));
        assert!(matches!(
            reach.overlap(&secret.join(".env"), false),
            Some(Overlap::Folder(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_reached_from_any_folder_on_its_device() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        let open = dir.path().join("open");
        std::fs::create_dir(&secret).unwrap();
        std::fs::create_dir(&open).unwrap();
        let file = secret.join(".env");
        std::fs::write(&file, "k").unwrap();
        let reach = Reach::of(&policy(&[&open], &[]));
        assert_eq!(
            reach.overlap(&file, false),
            None,
            "a single link stays apart"
        );
        std::fs::hard_link(&file, open.join("copy")).unwrap();
        assert!(matches!(
            reach.overlap(&file, false),
            Some(Overlap::HardLink(_))
        ));
    }
}

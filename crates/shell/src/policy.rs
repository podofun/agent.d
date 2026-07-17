//! Platform-agnostic sandbox policy: what a child process may touch.

use std::path::PathBuf;

use agentd_permissions::Permission;

/// Directory subtrees readable by any sandboxed child so the binary, libc, and
/// common config can be reached. Nonexistent entries are skipped at apply time.
pub const READ_BASELINE: &[&str] = &[
    "/usr",
    "/bin",
    "/lib",
    "/lib64",
    "/etc",
    "/opt",
    "/proc/self",
    "/dev/null",
    "/dev/zero",
    "/dev/urandom",
    "/dev/random",
];

/// Per-user read-only config a sandboxed child needs beyond the system dirs.
/// CLI tools read their global config on every invocation (`git`, `gh`, `npm`,
/// …), so without these a sandboxed tool warns `Permission denied` on its own
/// config even when the repo itself is granted. We expose the XDG config root
/// (`~/.config`) plus the common home dotfiles that live outside it, read-only.
///
/// Deliberately NOT included: credential stores (`~/.git-credentials`,
/// `~/.netrc`, `~/.ssh`, `~/.cargo/credentials.toml`). `~/.config` can itself
/// hold app secrets — exposing it read-only is the accepted tradeoff for making
/// ordinary tools work without a grant per tool. HOME-derived (so it can't be a
/// `const`); nonexistent entries are skipped at apply time.
pub fn user_read_baseline() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    vec![
        home.join(".config"),
        home.join(".gitconfig"),
        home.join(".npmrc"),
    ]
}

/// Always-writable scratch devices. Only real device nodes — `/dev/stdout` and
/// `/dev/stderr` are symlinks to pipe fds that Landlock rejects (EBADFD), and
/// the child inherits those fds already open, so a write rule is unnecessary.
pub const WRITE_SCRATCH: &[&str] = &["/dev/null"];

/// What a sandboxed child process is allowed to touch. Derived from the
/// execution's effective grants by the caller (the scripting shell binding).
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    /// Directory subtrees the child may read (fs.read grants; baseline added at apply).
    pub read_paths: Vec<PathBuf>,
    /// Directory subtrees the child may write (fs.write grants; scratch added at apply).
    pub write_paths: Vec<PathBuf>,
    /// Network master switch. true iff the execution holds ANY `net:` grant.
    /// false = no network at all (not even the proxy).
    pub allow_net: bool,
    /// Hosts the child may reach, as raw `net:<host>` grant slugs. The egress
    /// proxy checks a destination with `Permission::covers`, matching ctx.http
    /// exactly — no parallel match logic.
    pub net_hosts: Vec<Permission>,
    /// `shell.unrestricted` escape hatch. When true the sandbox is not applied.
    pub unrestricted: bool,
}

/// Outcome of trying to apply a sandbox policy to a process.
#[derive(Debug)]
pub enum SandboxError {
    /// No working backend on this platform/kernel; caller must fail closed.
    Unsupported,
    /// Backend failed to apply the policy.
    Apply(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Unsupported => {
                write!(
                    f,
                    "the native shell sandbox is not available on this platform"
                )
            }
            SandboxError::Apply(m) => write!(f, "could not apply the shell sandbox ({m})"),
        }
    }
}

impl std::error::Error for SandboxError {}

/// Collapse a permission specifier to its deepest concrete ancestor directory.
/// Kernel sandboxes confine by subtree, not glob; a specifier with `*`/`**` is
/// reduced to the path prefix before the first glob segment. Accepts both `/`
/// and `\` separators so Windows drive paths (`C:\dir\**`) collapse correctly.
pub fn concrete_ancestor(spec: &str) -> PathBuf {
    let Some(glob) = spec.find('*') else {
        // No glob: the whole specifier is already a concrete path.
        return PathBuf::from(spec);
    };
    // Drop the partial segment the glob sits in: back up to its separator.
    let head = &spec[..glob];
    let trimmed = match head.rfind(['/', '\\']) {
        Some(i) => &head[..i],
        None => "",
    };
    if trimmed.is_empty() {
        // Preserve the POSIX root for specifiers like `/**`.
        if spec.starts_with('/') {
            return PathBuf::from("/");
        }
        return PathBuf::new();
    }
    PathBuf::from(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn concrete_ancestor_strips_globs() {
        assert_eq!(concrete_ancestor("/allowed/**"), PathBuf::from("/allowed"));
        assert_eq!(
            concrete_ancestor("/tmp/project/*"),
            PathBuf::from("/tmp/project")
        );
        assert_eq!(
            concrete_ancestor("/var/log/app.log"),
            PathBuf::from("/var/log/app.log")
        );
        assert_eq!(concrete_ancestor("/a/*/b/**"), PathBuf::from("/a"));
    }

    #[test]
    fn concrete_ancestor_ignores_relative() {
        // Non-absolute specifiers are skipped by callers; helper returns as-is.
        assert_eq!(concrete_ancestor("relative/*"), PathBuf::from("relative"));
    }

    #[test]
    fn concrete_ancestor_handles_windows_separators() {
        // Windows grants arrive with backslashes and a drive letter.
        assert_eq!(
            concrete_ancestor(r"C:\Users\test\out\**"),
            PathBuf::from(r"C:\Users\test\out")
        );
        assert_eq!(
            concrete_ancestor(r"C:\Users\test\file.txt"),
            PathBuf::from(r"C:\Users\test\file.txt")
        );
        // Mixed separators (display path + appended `/**`).
        assert_eq!(
            concrete_ancestor(r"C:\Users\test/*"),
            PathBuf::from(r"C:\Users\test")
        );
    }

    #[test]
    fn net_hosts_default_empty() {
        let p = SandboxPolicy::default();
        assert!(p.net_hosts.is_empty());
        assert!(!p.allow_net);
    }

    #[test]
    fn user_read_baseline_covers_config_root_and_dotfiles() {
        // SAFETY: single-threaded test; we set HOME to a known value and read it
        // back through the helper.
        unsafe { std::env::set_var("HOME", "/home/tester") };
        let paths = user_read_baseline();
        assert!(paths.contains(&PathBuf::from("/home/tester/.config")));
        assert!(paths.contains(&PathBuf::from("/home/tester/.gitconfig")));
        assert!(paths.contains(&PathBuf::from("/home/tester/.npmrc")));
        // Credential stores must never be in the read baseline.
        assert!(!paths.contains(&PathBuf::from("/home/tester/.git-credentials")));
    }
}

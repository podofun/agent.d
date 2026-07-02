//! macOS Seatbelt backend. Enforced by wrapping argv in `sandbox-exec`, not by
//! self-restriction, so `apply` returns an error and `exec` calls `wrap_argv`.
//!
//! The no-network path is pure Seatbelt (`deny default`, no network-outbound
//! allow). Host-granular network is enforced by the transparent pf-broker path
//! (see [`super::macos_transparent`]): the child runs under a Seatbelt profile
//! that *allows* outbound ([`sbpl_net_for_broker`]) while `pf` — scoped to the
//! broker-leased uid — redirects its traffic to the daemon's policy relay.

use crate::policy::{READ_BASELINE, SandboxError, SandboxPolicy, WRITE_SCRATCH};

pub fn is_supported() -> bool {
    std::path::Path::new("/usr/bin/sandbox-exec").exists()
}

/// Host-granular net requires the pf broker to be reachable; support tracks it.
pub fn net_supported() -> bool {
    is_supported() && super::macos_transparent::broker_available()
}

/// Unused on macOS (wrapper model); kept for the dispatch signature parity.
pub fn apply(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::Apply(
        "macos uses argv wrapping; call wrap_argv".into(),
    ))
}

/// macOS-only read baseline on top of [`READ_BASELINE`]: dyld needs to stat
/// `/` itself (literal, not subpath — grants nothing below it) and read the
/// shared cache + system frameworks, or every child dies with SIGABRT before
/// `main`. Verified minimal on macOS 26: without the `/` literal, even
/// `/bin/echo` aborts.
const MACOS_READ_EXTRA: &[&str] = &[
    "/System",
    "/private/var/db",
    "/private/var/select", // `/bin/sh` reads its implementation through here
    "/dev/urandom",
    "/dev/random",
    "/dev/zero",
];

/// Seatbelt matches on RESOLVED paths, but grants often come in via symlinks
/// (`/var` -> `/private/var`, `/tmp` -> `/private/tmp` — every macOS tempdir).
/// Emit BOTH forms: the canonical one is what the kernel checks; the original
/// keeps the rule readable and covers non-resolving lookups.
fn path_forms(p: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = p.to_str().map(str::to_owned).into_iter().collect();
    if let Ok(c) = std::fs::canonicalize(p)
        && let Some(c) = c.to_str()
        && !out.iter().any(|o| o == c)
    {
        out.push(c.to_owned());
    }
    out
}

/// Shared SBPL prelude: default-deny + fs confinement from the policy. Network
/// rules are appended by the caller (none for the deny path).
///
/// `file-read-metadata` is allowed globally: stat/readlink only (no contents),
/// and required for symlink traversal (`/var`, `/tmp`) and the child's getcwd.
fn sbpl_fs(policy: &SandboxPolicy) -> String {
    let mut sbpl = String::from(
        "(version 1)\n(deny default)\n(allow process-exec* process-fork)\n(allow sysctl-read)\n\
         (allow file-read-metadata)\n(allow file-read* (literal \"/\"))\n",
    );
    for r in READ_BASELINE
        .iter()
        .chain(MACOS_READ_EXTRA.iter())
        .map(|s| s.to_string())
        .chain(policy.read_paths.iter().flat_map(|p| path_forms(p)))
    {
        sbpl.push_str(&format!("(allow file-read* (subpath \"{r}\"))\n"));
    }
    for w in policy
        .write_paths
        .iter()
        .flat_map(|p| path_forms(p))
        .chain(WRITE_SCRATCH.iter().map(|s| s.to_string()))
    {
        sbpl.push_str(&format!("(allow file-write* (subpath \"{w}\"))\n"));
    }
    sbpl
}

/// Generate SBPL and return the rewritten (bin, args) running under
/// `sandbox-exec`. Caller (exec on macOS) substitutes these for the original.
///
/// Network is fully denied: this path is only taken when the policy permits no
/// network (host-granular network has no transparent macOS backend yet, so
/// `allow_net` execs fail closed before reaching here).
pub fn wrap_argv(policy: &SandboxPolicy, bin: &str, args: &[String]) -> (String, Vec<String>) {
    // Network is default-denied (deny default + no network-outbound allow).
    let sbpl = sbpl_fs(policy);
    let mut new_args = vec!["-p".to_string(), sbpl, "--".to_string(), bin.to_string()];
    new_args.extend(args.iter().cloned());
    ("/usr/bin/sandbox-exec".to_string(), new_args)
}


/// SBPL for a broker-spawned net child: confine the filesystem but ALLOW
/// outbound network — `pf` (scoped to the child's dedicated uid) enforces the
/// host/IP allowlist, and the daemon's relay does the per-connection admit
/// decision. Built here, sent to the broker over the wire.
pub fn sbpl_net_for_broker(policy: &SandboxPolicy) -> String {
    let mut sbpl = sbpl_fs(policy);
    // mach-lookup: DNS on macOS goes through mDNSResponder's mach service.
    sbpl.push_str("(allow network-outbound)\n(allow network-bind)\n(allow mach-lookup)\n(allow system-socket)\n");
    sbpl
}


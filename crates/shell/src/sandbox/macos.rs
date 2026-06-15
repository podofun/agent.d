//! macOS Seatbelt backend. Enforced by wrapping argv in `sandbox-exec`, not by
//! self-restriction, so `apply` returns an error and `exec` calls `wrap_argv`.

use crate::policy::{READ_BASELINE, SandboxError, SandboxPolicy, WRITE_SCRATCH};

pub fn is_supported() -> bool {
    std::path::Path::new("/usr/bin/sandbox-exec").exists()
}

/// Unused on macOS (wrapper model); kept for the dispatch signature parity.
pub fn apply(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::Apply(
        "macos uses argv wrapping; call wrap_argv".into(),
    ))
}

/// Generate SBPL and return the rewritten (bin, args) running under
/// `sandbox-exec`. Caller (exec on macOS) substitutes these for the original.
pub fn wrap_argv(policy: &SandboxPolicy, bin: &str, args: &[String]) -> (String, Vec<String>) {
    let mut sbpl = String::from("(version 1)\n(deny default)\n(allow process-exec process-fork)\n");

    for r in READ_BASELINE
        .iter()
        .copied()
        .chain(policy.read_paths.iter().filter_map(|p| p.to_str()))
    {
        sbpl.push_str(&format!("(allow file-read* (subpath \"{r}\"))\n"));
    }
    for w in policy
        .write_paths
        .iter()
        .filter_map(|p| p.to_str())
        .chain(WRITE_SCRATCH.iter().copied())
    {
        sbpl.push_str(&format!("(allow file-write* (subpath \"{w}\"))\n"));
    }
    if policy.allow_net {
        sbpl.push_str("(allow network*)\n");
    }

    let mut new_args = vec!["-p".to_string(), sbpl, "--".to_string(), bin.to_string()];
    new_args.extend(args.iter().cloned());
    ("/usr/bin/sandbox-exec".to_string(), new_args)
}

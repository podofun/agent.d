//! macOS Seatbelt backend. Enforced by wrapping argv in `sandbox-exec`, not by
//! self-restriction, so `apply` returns an error and `exec` calls `wrap_argv`.
//!
//! Host-granular network (when the policy permits it) is enforced by `pf` scoped
//! to a dedicated UID (see [`super::macos_pf`]); the Seatbelt profile then
//! *allows* outbound and `pf` does the IP allowlisting.

use crate::policy::{READ_BASELINE, SandboxError, SandboxPolicy, WRITE_SCRATCH};
use crate::{ExecRequest, ExecResult, ShellError};

pub fn is_supported() -> bool {
    std::path::Path::new("/usr/bin/sandbox-exec").exists()
}

/// Net containment uses the same Seatbelt wrapper, so support tracks `is_supported`.
pub fn net_supported() -> bool {
    is_supported()
}

/// Unused on macOS (wrapper model); kept for the dispatch signature parity.
pub fn apply(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::Apply(
        "macos uses argv wrapping; call wrap_argv".into(),
    ))
}

/// Generate SBPL and return the rewritten (bin, args) running under
/// `sandbox-exec`. Caller (exec on macOS) substitutes these for the original.
///
/// Network is fully denied: this path is only taken when the policy permits no
/// network (host-granular network has no transparent macOS backend yet, so
/// `allow_net` execs fail closed before reaching here).
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
    // Network is default-denied (deny default + no network-outbound allow).

    let mut new_args = vec!["-p".to_string(), sbpl, "--".to_string(), bin.to_string()];
    new_args.extend(args.iter().cloned());
    ("/usr/bin/sandbox-exec".to_string(), new_args)
}

// ---- host-granular network path (pf + dedicated UID) ----

/// Dedicated sandbox UID for `pf` scoping, from `AGENTD_SANDBOX_UID` (the
/// operator provisions an unprivileged user). Without it, `pf` can't scope to a
/// single process tree, so the net sandbox fails closed.
fn sandbox_uid() -> Result<u32, ShellError> {
    std::env::var("AGENTD_SANDBOX_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            ShellError::Sandbox(
                "macOS net sandbox requires AGENTD_SANDBOX_UID (a dedicated unprivileged uid)"
                    .into(),
            )
        })
}

/// SBPL that confines the filesystem but ALLOWS outbound network — `pf` enforces
/// the host/IP allowlist for net children.
fn sbpl_net_allowed(policy: &SandboxPolicy) -> String {
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
    sbpl.push_str("(allow network-outbound)\n(allow network-bind)\n");
    sbpl
}

/// Pre-resolve the policy's net grants to IPs (concrete hostnames + literals;
/// wildcards are skipped — no L7 on this path).
fn preresolve(policy: &SandboxPolicy) -> Vec<std::net::IpAddr> {
    use crate::dns_pin::{Resolve, SystemResolver, split_grants};
    let (host_grants, mut ips) = split_grants(&policy.net_hosts);
    let resolver = SystemResolver;
    for g in &host_grants {
        if let ("net", Some(name)) = g.parts()
            && !name.contains('*')
            && let Ok(addrs) = resolver.resolve(name)
        {
            ips.extend(addrs);
        }
    }
    ips
}

/// Run `req` with host-granular network: `pf` allowlist scoped to a dedicated
/// UID; the child runs as that UID under Seatbelt (fs) with outbound allowed.
pub async fn run_contained(
    req: &ExecRequest,
    policy: &SandboxPolicy,
) -> Result<ExecResult, ShellError> {
    use super::macos_pf::PfFilter;
    use crate::netfilter::NetFilter;
    use std::process::Stdio;

    let uid = sandbox_uid()?;
    let filter = PfFilter::new(uid);
    let ips = preresolve(policy);
    let handle = filter
        .provision(&ips)
        .map_err(|e| ShellError::Sandbox(format!("pf provision: {e}")))?;

    let sbpl = sbpl_net_allowed(policy);
    let mut argv = vec!["-p".to_string(), sbpl, "--".to_string(), req.bin.clone()];
    argv.extend(req.args.iter().cloned());

    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.args(&argv);
    if let Some(cwd) = &req.cwd {
        cmd.current_dir(cwd);
    }
    // Drop to the dedicated UID in the forked child before exec, so `pf`'s
    // `user`-match scopes to exactly this process tree.
    // SAFETY: pre_exec runs in the forked child; raw setgid/setuid syscalls only.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setgid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let result = run_cmd(cmd, req).await;
    filter.teardown(handle);
    result
}

async fn run_cmd(
    mut cmd: tokio::process::Command,
    req: &ExecRequest,
) -> Result<ExecResult, ShellError> {
    use tokio::io::AsyncWriteExt;
    let mut child = cmd.spawn().map_err(|e| ShellError::Spawn {
        bin: req.bin.clone(),
        source: e,
    })?;
    if let (Some(input), Some(mut stdin)) = (req.stdin.as_ref(), child.stdin.take()) {
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await?;
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().await?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr_text = String::from_utf8_lossy(&output.stderr).into_owned();
    let exit_code = output.status.code().unwrap_or(-1);
    let (stdout, stderr) = if req.separate_stderr {
        (stdout, stderr_text)
    } else {
        let mut merged = stdout;
        if !stderr_text.is_empty() {
            if !merged.is_empty() && !merged.ends_with('\n') {
                merged.push('\n');
            }
            merged.push_str(&stderr_text);
        }
        (merged, String::new())
    };
    Ok(ExecResult {
        exit_code,
        stdout,
        stderr,
    })
}

//! macOS transparent network backend: `pf` IP allowlist scoped to a dedicated
//! UID (the macOS analog of Linux's netns identity).
//!
//! Model:
//! - The sandboxed child runs under a **dedicated UID**. `pf` can match rules by
//!   `user <uid>`, which is our per-process scope (macOS has no netns).
//! - A `pf` anchor for that UID **default-denies** outbound and permits only IPs
//!   in a live `pf` **table** (`pfctl ... -T add`), plus UDP/53 for DNS.
//! - DNS-pin is daemon-side **pre-resolution** (like the Windows backend): the
//!   policy's `net:<host>` grants are resolved up front and their IPs added to
//!   the table; literal `net:<ip>` grants go in directly.
//!
//! This is driver-free and uses the `pfctl` CLI (no `/dev/pf` ioctls), so it is
//! almost entirely safe `Command` code. It needs root to manage `pf` (a
//! privileged helper / the daemon running elevated).

#![cfg(target_os = "macos")]

use std::io::Write;
use std::net::IpAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::netfilter::{FilterError, FilterHandle, NetFilter, Supports};

/// A live `pf` anchor + table for one sandboxed identity (named after its UID).
pub struct PfHandle {
    anchor: String,
    table: String,
}

impl FilterHandle for PfHandle {}

/// `pf`-backed filter scoped to a dedicated UID.
pub struct PfFilter {
    uid: u32,
}

impl PfFilter {
    pub fn new(uid: u32) -> Self {
        PfFilter { uid }
    }

    fn anchor(&self) -> String {
        format!("agentd/sbx_{}", self.uid)
    }
    fn table(&self) -> String {
        format!("agentd_sbx_{}", self.uid)
    }
}

fn pf_err(what: &str, detail: impl std::fmt::Display) -> FilterError {
    FilterError::Apply(format!("pf {what}: {detail}"))
}

/// Run `pfctl` with `args`, optionally piping `stdin`. Returns the exit success.
fn pfctl(args: &[&str], stdin: Option<&str>) -> Result<(), FilterError> {
    let mut c = Command::new("pfctl");
    c.args(args).stdout(Stdio::null()).stderr(Stdio::piped());
    if stdin.is_some() {
        c.stdin(Stdio::piped());
    }
    let mut child = c.spawn().map_err(|e| pf_err("spawn", e))?;
    if let (Some(s), Some(mut sin)) = (stdin, child.stdin.take()) {
        sin.write_all(s.as_bytes())
            .map_err(|e| pf_err("stdin", e))?;
    }
    let out = child.wait_with_output().map_err(|e| pf_err("wait", e))?;
    if !out.status.success() {
        return Err(pf_err(
            "pfctl",
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(())
}

impl NetFilter for PfFilter {
    type Handle = PfHandle;

    fn supports(&self) -> Supports {
        Supports {
            ipv4: true,
            ipv6: true,
        }
    }

    fn provision(&self, literal_ips: &[IpAddr]) -> Result<PfHandle, FilterError> {
        let anchor = self.anchor();
        let table = self.table();
        // Anchor ruleset: default-deny outbound for the UID, permit the table +
        // DNS. pf evaluates last-match, so the permits follow the block.
        let ruleset = format!(
            "table <{table}> persist\n\
             block drop out proto {{ tcp udp }} user {uid}\n\
             pass out proto udp user {uid} to any port 53 keep state\n\
             pass out proto {{ tcp udp }} user {uid} to <{table}> keep state\n",
            uid = self.uid,
        );
        // Load the anchor.
        pfctl(&["-a", &anchor, "-f", "-"], Some(&ruleset))?;
        // Ensure pf is enabled (idempotent; `-e` errors if already enabled).
        let _ = pfctl(&["-e"], None);

        let h = PfHandle { anchor, table };
        if !literal_ips.is_empty() {
            self.commit_allow(&h, literal_ips, Duration::ZERO)?;
        }
        Ok(h)
    }

    fn commit_allow(
        &self,
        handle: &PfHandle,
        ips: &[IpAddr],
        _ttl: Duration,
    ) -> Result<(), FilterError> {
        if ips.is_empty() {
            return Ok(());
        }
        let mut args = vec![
            "-a",
            handle.anchor.as_str(),
            "-t",
            handle.table.as_str(),
            "-T",
            "add",
        ];
        let ip_strs: Vec<String> = ips.iter().map(|i| i.to_string()).collect();
        for s in &ip_strs {
            args.push(s.as_str());
        }
        pfctl(&args, None)
    }

    fn revoke(&self, handle: &PfHandle, ips: &[IpAddr]) -> Result<(), FilterError> {
        if ips.is_empty() {
            return Ok(());
        }
        let ip_strs: Vec<String> = ips.iter().map(|i| i.to_string()).collect();
        let mut args = vec![
            "-a",
            handle.anchor.as_str(),
            "-t",
            handle.table.as_str(),
            "-T",
            "delete",
        ];
        for s in &ip_strs {
            args.push(s.as_str());
        }
        pfctl(&args, None)
    }

    fn teardown(&self, handle: PfHandle) {
        // Flush the table and the anchor ruleset (best-effort).
        let _ = pfctl(
            &["-a", &handle.anchor, "-t", &handle.table, "-T", "flush"],
            None,
        );
        let _ = pfctl(&["-a", &handle.anchor, "-F", "rules"], None);
    }
}

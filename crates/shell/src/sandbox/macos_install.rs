//! One-time privileged setup for the macOS sandbox (`sudo agentd
//! --install-sandbox`). This is the ONLY code that runs elevated; after it, the
//! daemon runs unprivileged and reaches root-only operations solely through the
//! broker.
//!
//! It creates the dedicated sandbox users, installs the broker binary + its
//! launchd job, writes the broker config, loads the pf anchor hooks, and sets
//! the sysctls that `route-to` needs. `uninstall()` reverses all of it.

#![cfg(target_os = "macos")]

use std::process::Command;

use super::macos_broker::config::BrokerConfig;
use super::macos_broker::pool::SandboxUser;
use super::macos_pf_rules::{MAIN_CONF, REQUIRED_SYSCTLS};
use crate::policy::SandboxError;

const GROUP: &str = "_agentd_sbx";
const NUM_USERS: u32 = 4;
const UID_BASE: u32 = 700;
const CONF_DIR: &str = "/etc/agentd";
const CONF_PATH: &str = "/etc/agentd/broker.conf";
const HELPER_DIR: &str = "/Library/PrivilegedHelperTools";
const HELPER_PATH: &str = "/Library/PrivilegedHelperTools/agentd-pf-broker";
const PLIST_PATH: &str = "/Library/LaunchDaemons/fun.podo.agentd-pf-broker.plist";
const PF_CONF_PATH: &str = "/etc/agentd/pf.conf";
const SYSCTL_CONF: &str = "/etc/agentd/sysctl-forwarding.conf";
const LABEL: &str = "fun.podo.agentd-pf-broker";

fn err(msg: impl Into<String>) -> SandboxError {
    SandboxError::Apply(msg.into())
}

fn require_root() -> Result<(), SandboxError> {
    // SAFETY: geteuid(2) takes no arguments, cannot fail, and returns the
    // effective uid. std exposes no equivalent, so libc is the only option.
    if unsafe { libc::geteuid() } != 0 {
        return Err(err(
            "macOS sandbox setup needs root: run `sudo agentd --install-sandbox`",
        ));
    }
    Ok(())
}

/// The uid that invoked sudo — the daemon's unprivileged identity, the only
/// peer the broker will accept.
fn daemon_uid() -> Result<u32, SandboxError> {
    std::env::var("SUDO_UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| err("cannot determine invoking uid (SUDO_UID unset); run via sudo"))
}

fn run(bin: &str, args: &[&str]) -> Result<(), SandboxError> {
    let out = Command::new(bin)
        .args(args)
        .output()
        .map_err(|e| err(format!("{bin}: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(err(format!(
            "{bin} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

/// Whether a dscl path already exists (idempotent create).
fn dscl_exists(path: &str) -> bool {
    Command::new("/usr/bin/dscl")
        .args([".", "-read", path])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ensure_group() -> Result<u32, SandboxError> {
    let gpath = format!("/Groups/{GROUP}");
    let gid = UID_BASE; // reuse the base as the shared gid
    if !dscl_exists(&gpath) {
        run("/usr/bin/dscl", &[".", "-create", &gpath])?;
        run("/usr/bin/dscl", &[".", "-create", &gpath, "PrimaryGroupID", &gid.to_string()])?;
    }
    Ok(gid)
}

fn ensure_users(gid: u32) -> Result<Vec<SandboxUser>, SandboxError> {
    let mut users = Vec::new();
    for i in 0..NUM_USERS {
        let uid = UID_BASE + i;
        let name = format!("{GROUP}{i}");
        let upath = format!("/Users/{name}");
        if !dscl_exists(&upath) {
            run("/usr/bin/dscl", &[".", "-create", &upath])?;
            run("/usr/bin/dscl", &[".", "-create", &upath, "UniqueID", &uid.to_string()])?;
            run("/usr/bin/dscl", &[".", "-create", &upath, "PrimaryGroupID", &gid.to_string()])?;
            run("/usr/bin/dscl", &[".", "-create", &upath, "UserShell", "/usr/bin/false"])?;
            run("/usr/bin/dscl", &[".", "-create", &upath, "NFSHomeDirectory", "/var/empty"])?;
            run("/usr/bin/dscl", &[".", "-create", &upath, "IsHidden", "1"])?;
        }
        users.push(SandboxUser { uid, name });
    }
    Ok(users)
}

fn plist(daemon_uid: u32) -> String {
    // KeepAlive so a broker crash is restarted; runs as root (the whole point).
    // daemon_uid is embedded only as a comment aid — the broker reads it from
    // the config file, not argv.
    let _ = daemon_uid;
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{HELPER_PATH}</string></array>
  <key>KeepAlive</key><true/>
  <key>RunAtLoad</key><true/>
  <key>ProcessType</key><string>Adaptive</string>
</dict>
</plist>
"#
    )
}

/// Install everything. Idempotent: safe to re-run.
pub fn install() -> Result<(), SandboxError> {
    require_root()?;
    let duid = daemon_uid()?;

    let gid = ensure_group()?;
    let users = ensure_users(gid)?;

    // Config dir + broker.conf.
    std::fs::create_dir_all(CONF_DIR).map_err(|e| err(format!("mkdir {CONF_DIR}: {e}")))?;
    let cfg = BrokerConfig { daemon_uid: duid, users: users.clone() };
    std::fs::write(CONF_PATH, cfg.render()).map_err(|e| err(format!("write {CONF_PATH}: {e}")))?;

    // Broker binary: copy from beside the running executable.
    let src = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("agentd-pf-broker")))
        .filter(|p| p.exists())
        .ok_or_else(|| err("agentd-pf-broker not found beside the agentd binary"))?;
    std::fs::create_dir_all(HELPER_DIR).map_err(|e| err(format!("mkdir {HELPER_DIR}: {e}")))?;
    std::fs::copy(&src, HELPER_PATH).map_err(|e| err(format!("copy broker: {e}")))?;
    run("/bin/chmod", &["755", HELPER_PATH])?;
    run("/usr/sbin/chown", &["root:wheel", HELPER_PATH])?;

    // pf: write our main conf and load it (preserves com.apple anchors + adds
    // agentd hooks), then enable pf.
    std::fs::write(PF_CONF_PATH, MAIN_CONF).map_err(|e| err(format!("write {PF_CONF_PATH}: {e}")))?;
    run("/sbin/pfctl", &["-f", PF_CONF_PATH])?;
    let _ = Command::new("/sbin/pfctl").arg("-e").output(); // -e errors if already enabled

    // sysctls now + persisted for reboot.
    let mut persisted = String::new();
    for (k, v) in REQUIRED_SYSCTLS {
        run("/usr/sbin/sysctl", &["-w", &format!("{k}={v}")])?;
        persisted.push_str(&format!("{k}={v}\n"));
    }
    std::fs::write(SYSCTL_CONF, persisted).map_err(|e| err(format!("write {SYSCTL_CONF}: {e}")))?;

    // launchd job.
    std::fs::write(PLIST_PATH, plist(duid)).map_err(|e| err(format!("write {PLIST_PATH}: {e}")))?;
    run("/usr/sbin/chown", &["root:wheel", PLIST_PATH])?;
    run("/bin/chmod", &["644", PLIST_PATH])?;
    // bootout first (ignore failure) so re-install reloads cleanly.
    let _ = Command::new("/bin/launchctl").args(["bootout", "system", PLIST_PATH]).output();
    run("/bin/launchctl", &["bootstrap", "system", PLIST_PATH])?;

    Ok(())
}

/// Reverse `install()`. Best-effort: continues past individual failures so a
/// partial install can always be cleaned up.
pub fn uninstall() -> Result<(), SandboxError> {
    require_root()?;
    let _ = Command::new("/bin/launchctl").args(["bootout", "system", PLIST_PATH]).output();
    let _ = std::fs::remove_file(PLIST_PATH);
    let _ = std::fs::remove_file(HELPER_PATH);
    // Flush any agentd anchors and reload stock pf.
    for i in 0..NUM_USERS {
        let anchor = super::macos_pf_rules::anchor_name(UID_BASE + i);
        let _ = Command::new("/sbin/pfctl").args(["-a", &anchor, "-F", "all"]).output();
    }
    let _ = Command::new("/sbin/pfctl").args(["-f", "/etc/pf.conf"]).output();
    // Remove users + group.
    for i in 0..NUM_USERS {
        let _ = Command::new("/usr/bin/dscl")
            .args([".", "-delete", &format!("/Users/{GROUP}{i}")])
            .output();
    }
    let _ = Command::new("/usr/bin/dscl").args([".", "-delete", &format!("/Groups/{GROUP}")]).output();
    let _ = std::fs::remove_file(CONF_PATH);
    let _ = std::fs::remove_file(PF_CONF_PATH);
    let _ = std::fs::remove_file(SYSCTL_CONF);
    Ok(())
}

//! Client side of the macOS pf-broker split (daemon = unprivileged; the root
//! `agentd-pf-broker` launchd daemon does the four root-only operations: pf
//! anchor load, DIOCNATLOOK, spawn-as-sandbox-uid, ACL stamping).
//!
//! The wire protocol lives in [`proto`] and is unit-tested on every unix OS;
//! the connected client and `broker_available()` are macOS-only.

pub mod proto;

/// Filesystem path of the broker's unix socket (created root-owned by the
/// launchd daemon; connect access is what `getpeereid` guards).
pub const SOCKET_PATH: &str = "/var/run/agentd/broker.sock";

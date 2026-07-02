//! Landlock backend: filesystem confinement (network is enforced separately via
//! the netns supervisor + nftables, see `linux_transparent`/`netfilter`).
//!
//! `apply` builds the ruleset and calls `restrict_self` on the CALLING thread,
//! so it must run from the forked child (via `pre_exec`) — never the daemon.

use std::path::PathBuf;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus,
};

use crate::policy::{READ_BASELINE, SandboxError, SandboxPolicy, WRITE_SCRATCH};

fn apply_err<E: std::fmt::Display>(e: E) -> SandboxError {
    SandboxError::Apply(e.to_string())
}

/// Probe whether the running kernel enforces Landlock, without restricting the
/// caller. Uses HardRequirement so a kernel lacking Landlock ABI v1 errors
/// rather than silently producing a no-op ruleset.
pub fn is_supported() -> bool {
    Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .and_then(|r| r.create())
        .is_ok()
}

/// Whether host-granular net containment (rootless netns) can be enforced here.
pub fn net_supported() -> bool {
    super::linux_net::userns_net_supported()
}

/// Build and enforce the ruleset on the current thread. Call from the forked
/// child so only the child is confined.
pub fn apply(policy: &SandboxPolicy) -> Result<(), SandboxError> {
    let abi = ABI::V5;
    let read = AccessFs::from_read(abi);
    let all = AccessFs::from_all(abi);

    let mut ruleset = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(all)
        .map_err(apply_err)?;

    // Coarse network: when not allowed, handle the TCP accesses and add no port
    // rules, which denies all bind/connect. When allowed, leave network
    // unhandled so it stays unrestricted. (Best-effort: downgraded on kernels
    // without Landlock network support.)
    if !policy.allow_net {
        ruleset = ruleset
            .handle_access(AccessNet::BindTcp)
            .map_err(apply_err)?;
        ruleset = ruleset
            .handle_access(AccessNet::ConnectTcp)
            .map_err(apply_err)?;
    }

    let mut created = ruleset.create().map_err(apply_err)?;

    // Read subtrees: baseline + granted reads + granted writes (writable dirs
    // must also be readable). Nonexistent paths are skipped.
    let read_dirs = READ_BASELINE
        .iter()
        .map(PathBuf::from)
        .chain(policy.read_paths.iter().cloned())
        .chain(policy.write_paths.iter().cloned());
    for path in read_dirs {
        if let Ok(fd) = PathFd::new(&path) {
            created = created
                .add_rule(PathBeneath::new(fd, read))
                .map_err(apply_err)?;
        }
    }

    // Write subtrees: granted writes + scratch devices.
    let write_dirs = policy
        .write_paths
        .iter()
        .cloned()
        .chain(WRITE_SCRATCH.iter().map(PathBuf::from));
    for path in write_dirs {
        if let Ok(fd) = PathFd::new(&path) {
            created = created
                .add_rule(PathBeneath::new(fd, all))
                .map_err(apply_err)?;
        }
    }

    let status = created.restrict_self().map_err(apply_err)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(SandboxError::Unsupported);
    }

    // Landlock's network access only covers TCP. When network is denied, add a
    // seccomp filter that blocks creating IP sockets entirely, closing the
    // UDP/raw egress that Landlock alone leaves open.
    if !policy.allow_net {
        super::seccomp_linux::deny_ip_sockets()?;
    }
    Ok(())
}

//! Windows AppContainer backend.
//!
//! AppContainer enforcement happens at spawn (the child is launched under an
//! AppContainer SID with ACL grants on read/write paths and the
//! `internetClient` capability when `allow_net`), not via self-restriction.
//! That spawn integration is owned by this module and wired into `exec`'s
//! windows path.
//!
//! Phase 1 status: the full `CreateAppContainerProfile` + ACL grant + capability
//! spawn path is tracked as remaining work for the Windows target. Until it
//! lands, `is_supported` reports false so `ctx.shell` fails closed on Windows
//! rather than running unconfined. See the Release Gate in the design spec —
//! the feature is not shippable for Windows until this is implemented and its
//! CI is green.

use crate::policy::{SandboxError, SandboxPolicy};

pub fn is_supported() -> bool {
    // Flip to true once the restricted-token + firewall/WFP path is implemented.
    false
}

/// Net containment (restricted token + sandbox user + firewall/WFP) is not yet
/// implemented; fail closed.
pub fn net_supported() -> bool {
    false
}

pub fn apply(_policy: &SandboxPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::Unsupported)
}

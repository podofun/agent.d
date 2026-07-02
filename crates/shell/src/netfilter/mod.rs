//! Transparent host/IP-granular network enforcement.
//!
//! The `NetFilter` backend trait is the OS-agnostic seam: the shared core (the
//! DNS-pinning resolver + the [`permitset`] bookkeeping) decides *which* IPs a
//! sandboxed child may reach; a backend mirrors that decision into a per-process
//! kernel mechanism (Linux nftables set, macOS pf table, Windows WFP filters).
//!
//! Enforcement is **default-deny, all-families (IPv4 + IPv6), all protocols**:
//! a backend that cannot enforce a family or protocol must deny it, never pass
//! it (see [`NetFilter::supports`]). The only permitted destinations are the
//! resolver endpoint and the IPs in the per-identity permitted set.

use std::net::IpAddr;
use std::time::Duration;

pub mod permitset;

#[cfg(target_os = "linux")]
pub mod nftables;

#[cfg(test)]
pub mod mock;

pub use permitset::{Added, PermitSet, SetConfig};

/// Opaque per-identity handle returned by [`NetFilter::provision`]. Owns the
/// process identity (netns / dedicated UID / AppContainer SID) and an isolated
/// permitted sub-table. Dropping it via [`NetFilter::teardown`] removes all
/// filter state for that identity.
pub trait FilterHandle: Send + Sync {}

/// Error from a backend operation.
#[derive(Debug, thiserror::Error)]
pub enum FilterError {
    #[error("network filter backend unavailable: {0}")]
    Unsupported(String),
    #[error("filter operation failed: {0}")]
    Apply(String),
    #[error("permitted set full (max_size reached)")]
    Full,
}

/// A per-OS transparent network-enforcement backend.
///
/// Contract:
/// - [`provision`](NetFilter::provision) creates an identity with a default-deny,
///   all-family permitted set seeded with the policy's literal `net:<ip>` grants.
/// - [`commit_allow`](NetFilter::commit_allow) is **durable-before-return**: when
///   it returns `Ok`, the IPs are in effect in the kernel. The DNS-pin ordering
///   (commit before answering the child's query) depends on this.
/// - Each handle owns an **isolated** permitted sub-table; one exec's grants
///   never widen another's.
/// - [`teardown`](NetFilter::teardown) leaves no filter state; safe to call on an
///   orphan sweep.
pub trait NetFilter: Send + Sync {
    type Handle: FilterHandle;

    /// Whether this backend can default-deny the given family on this host. A
    /// backend returning `false` for any family in use must be treated as
    /// unavailable (fail closed, never pass the family).
    fn supports(&self) -> Supports;

    /// Provision an identity + default-deny set seeded with `literal_ips`.
    fn provision(&self, literal_ips: &[IpAddr]) -> Result<Self::Handle, FilterError>;

    /// Add `ips` to this handle's permitted set for `ttl`. Durable before return.
    fn commit_allow(
        &self,
        handle: &Self::Handle,
        ips: &[IpAddr],
        ttl: Duration,
    ) -> Result<(), FilterError>;

    /// Remove `ips` (TTL sweep / teardown). Best-effort durable.
    fn revoke(&self, handle: &Self::Handle, ips: &[IpAddr]) -> Result<(), FilterError>;

    /// Tear down the identity and its permitted set. Idempotent.
    fn teardown(&self, handle: Self::Handle);
}

/// Which address families a backend can default-deny on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Supports {
    pub ipv4: bool,
    pub ipv6: bool,
}

impl Supports {
    /// A backend is usable only if it can deny every family that exists on the
    /// host. We require both v4 and v6 coverage: partial coverage is a bypass.
    pub fn is_usable(&self) -> bool {
        self.ipv4 && self.ipv6
    }
}

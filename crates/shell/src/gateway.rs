//! Host-side transparent gateway for the Linux netns backend (Architecture B).
//!
//! The rootless netns has no real egress (no pasta/slirp), so the child's
//! traffic is nft-REDIRECTed to an in-namespace intercept and bridged to the
//! host over two AF_UNIX socketpairs created before `unshare`:
//!
//! - **ctrl** (SCM_RIGHTS): each intercepted TCP connection's fd + original
//!   destination is passed to the host, which enforces the IP allowlist and
//!   splices to the real destination.
//! - **dns**: each child DNS query is forwarded to the host, which resolves it
//!   under the pin policy (commit the resolved IPs to the permitted set BEFORE
//!   replying), and the answer is sent back for the supervisor to return to the
//!   child.
//!
//! Enforcement is the shared [`PermitSet`]: a DNS answer for an allowed name
//! adds its IPs; the TCP relay admits a connection iff its original-destination
//! IP is in the set. This module holds the pure host-side logic (DNS handling +
//! the admit decision); the socket plumbing lives in `sandbox::linux_net`.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd_permissions::Permission;

use crate::dns_pin::{Resolve, name_allowed};
use crate::dns_wire;
use crate::netfilter::PermitSet;

/// Shared, live permitted-IP set for one sandboxed identity.
pub type SharedSet = Arc<Mutex<PermitSet>>;

/// Process one DNS query packet from the child under the pin policy. Returns the
/// response bytes to send back, or `None` if the query is malformed (drop it).
///
/// Ordering invariant: for an allowed name, the resolved IPs are committed to
/// `set` BEFORE the response is returned, so the child cannot `connect()` to an
/// IP the relay has not yet been told to admit.
pub fn handle_dns(
    query: &[u8],
    host_grants: &[Permission],
    set: &SharedSet,
    resolver: &dyn Resolve,
    ttl: Duration,
) -> Option<Vec<u8>> {
    let q = dns_wire::parse_query(query)?;
    if !name_allowed(host_grants, &q.name) {
        return Some(dns_wire::build_nxdomain(&q));
    }
    let ips = resolver.resolve(&q.name).unwrap_or_default();
    if !ips.is_empty() {
        let mut s = set.lock().unwrap();
        let now = Instant::now();
        for ip in &ips {
            s.allow_resolved(*ip, ttl, now);
        }
        // IPs are in the set before this function returns, so the relay admits
        // them the moment the child receives the answer.
    }
    Some(dns_wire::build_response(&q, &ips, ttl.as_secs() as u32))
}

/// Whether a TCP connection to `dst` should be admitted: its destination IP must
/// be in the permitted set (literal grant or a pinned host IP).
pub fn admit(dst: IpAddr, set: &SharedSet) -> bool {
    set.lock().unwrap().contains(&dst, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dns_wire::{TYPE_A, build_query, parse_answers};
    use crate::netfilter::SetConfig;
    use std::collections::HashMap;

    struct MapResolver(HashMap<String, Vec<IpAddr>>);
    impl Resolve for MapResolver {
        fn resolve(&self, name: &str) -> std::io::Result<Vec<IpAddr>> {
            Ok(self.0.get(name).cloned().unwrap_or_default())
        }
    }

    fn perm(s: &str) -> Permission {
        Permission::new(s)
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    fn shared() -> SharedSet {
        Arc::new(Mutex::new(PermitSet::new(SetConfig::default())))
    }

    #[test]
    fn allowed_query_resolves_commits_then_answers() {
        let set = shared();
        let mut map = HashMap::new();
        map.insert("api.example.com".to_string(), vec![ip("93.184.216.34")]);
        let up = MapResolver(map);
        let q = build_query(1, "api.example.com", TYPE_A);

        let resp = handle_dns(
            &q,
            &[perm("net:api.example.*")],
            &set,
            &up,
            Duration::from_secs(60),
        )
        .unwrap();

        // IP is admitted the instant the response is built (commit-before-answer).
        assert!(admit(ip("93.184.216.34"), &set));
        assert_eq!(parse_answers(&resp), vec![ip("93.184.216.34")]);
    }

    #[test]
    fn denied_query_is_nxdomain_and_admits_nothing() {
        let set = shared();
        let mut map = HashMap::new();
        map.insert("evil.com".to_string(), vec![ip("6.6.6.6")]);
        let up = MapResolver(map);
        let q = build_query(1, "evil.com", TYPE_A);

        let resp = handle_dns(
            &q,
            &[perm("net:api.example.*")],
            &set,
            &up,
            Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(resp[3] & 0x0F, 3); // NXDOMAIN
        assert!(!admit(ip("6.6.6.6"), &set));
    }

    #[test]
    fn admit_requires_prior_resolution() {
        let set = shared();
        // Nothing resolved yet: deny.
        assert!(!admit(ip("1.2.3.4"), &set));
    }

    #[test]
    fn literal_ip_in_set_is_admitted() {
        let set = shared();
        set.lock().unwrap().allow_literal(ip("203.0.113.7"));
        assert!(admit(ip("203.0.113.7"), &set));
        assert!(!admit(ip("203.0.113.8"), &set));
    }

    #[test]
    fn malformed_query_dropped() {
        let set = shared();
        let up = MapResolver(HashMap::new());
        assert!(handle_dns(&[0u8; 3], &[], &set, &up, Duration::from_secs(60)).is_none());
    }
}

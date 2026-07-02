//! DNS-pinning resolver: the child's only name path.
//!
//! Grants arrive as `net:<host>` / `net:<ip>` slugs (`SandboxPolicy::net_hosts`).
//! Literal-IP grants are seeded straight into the filter's permitted set at
//! provision time. Host grants are resolved on demand: when the sandboxed child
//! queries an allowed name, we resolve it upstream, **commit the resulting IPs
//! to the filter (durable) before returning the answer** — closing the window
//! where a `connect()` could race ahead of the filter update — and return the
//! pinned answer. A name no grant covers returns `NxDomain`.
//!
//! This module is the pure policy core: upstream resolution is injected via the
//! [`Resolve`] trait so it is fully unit-testable without real DNS.

use std::net::IpAddr;

use agentd_permissions::Permission;

/// Upstream name resolution, injected for testability. Implementors return the
/// A/AAAA addresses for a name, or an empty vec / error if it does not resolve.
pub trait Resolve: Send + Sync {
    fn resolve(&self, name: &str) -> std::io::Result<Vec<IpAddr>>;
}

/// Upstream resolution via the host stdlib resolver (`getaddrinfo`). Used in
/// production; tests inject a deterministic [`Resolve`] instead. Port 0 is used
/// only to satisfy the `host:port` form the stdlib resolver expects.
pub struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: &str) -> std::io::Result<Vec<IpAddr>> {
        use std::net::ToSocketAddrs;
        let addrs = (name, 0u16).to_socket_addrs()?;
        Ok(addrs.map(|sa| sa.ip()).collect())
    }
}

/// Split `net:` grant slugs into literal-IP grants (seeded into the filter) and
/// host-name grants (matched at resolve time). A slug whose specifier parses as
/// an `IpAddr` is a literal; everything else (including wildcards like
/// `net:*.example.com`) is a host grant.
pub fn split_grants(net_hosts: &[Permission]) -> (Vec<Permission>, Vec<IpAddr>) {
    let mut hosts = Vec::new();
    let mut literals = Vec::new();
    for p in net_hosts {
        match p.parts() {
            ("net", Some(spec)) => match spec.parse::<IpAddr>() {
                Ok(ip) => literals.push(ip),
                Err(_) => hosts.push(p.clone()),
            },
            // Bare `net` (covers everything) is treated as a host grant so the
            // covers() check below admits any name; no literal seeding.
            ("net", None) => hosts.push(p.clone()),
            _ => {}
        }
    }
    (hosts, literals)
}

/// Whether any host grant covers `name`, using the exact `Permission::covers`
/// semantics the permission engine uses (so `net:*.example.com`, `net:*`, and
/// exact matches behave identically to `ctx.http`).
pub fn name_allowed(host_grants: &[Permission], name: &str) -> bool {
    let want = Permission::new(format!("net:{}", name.to_ascii_lowercase()));
    host_grants.iter().any(|g| g.covers(&want))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perm(s: &str) -> Permission {
        Permission::new(s)
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn split_separates_literals_from_hosts() {
        let grants = vec![
            perm("net:google.com"),
            perm("net:53.32.42.234"),
            perm("net:api.example.*"),
            perm("net:2606:4700::1111"),
        ];
        let (hosts, literals) = split_grants(&grants);
        assert_eq!(hosts.len(), 2); // google.com, api.example.*
        assert_eq!(literals, vec![ip("53.32.42.234"), ip("2606:4700::1111")]);
    }

    #[test]
    fn name_allowed_uses_covers_semantics() {
        // net wildcards are suffix-style (`prefix*`), matching Permission::covers
        // exactly — same semantics as ctx.http. Subdomain `*.x` is not a form.
        let grants = vec![perm("net:google.com"), perm("net:api.example.*")];
        assert!(name_allowed(&grants, "google.com"));
        assert!(name_allowed(&grants, "api.example.com")); // suffix wildcard
        assert!(name_allowed(&grants, "api.example.org")); // suffix wildcard
        assert!(name_allowed(&grants, "GOOGLE.COM")); // case-insensitive
        assert!(!name_allowed(&grants, "evil.com"));
        // `net:*` covers everything.
        assert!(name_allowed(&[perm("net:*")], "anything.test"));
    }

    #[test]
    fn literal_ip_grant_split_out() {
        let (hosts, literals) = split_grants(&[perm("net:53.32.42.234"), perm("net:x.test")]);
        assert_eq!(literals, vec![ip("53.32.42.234")]);
        assert_eq!(hosts.len(), 1);
    }
}

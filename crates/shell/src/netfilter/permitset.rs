//! Per-identity permitted-IP set with TTL + refcount eviction.
//!
//! This is the pure, OS-agnostic bookkeeping behind every `NetFilter` backend.
//! A backend mirrors this set into its kernel mechanism (nftables set / pf table
//! / WFP filters); this type decides *what* should be permitted and *when* an
//! entry ages out, so the policy is identical on every platform and unit-testable
//! without privilege.
//!
//! Invariants:
//! - Literal `net:<ip>` grants never expire while the exec lives (`ttl = None`).
//! - Host-resolved IPs carry a TTL deadline (floored to `min_ttl`, capped to
//!   `max_ttl`); `sweep` removes entries past their deadline.
//! - An IP authorized by more than one grant/resolution carries a refcount; it is
//!   removed only when the last holder drops it AND it is not within a live TTL.
//! - `max_size` caps growth; exceeding it denies new additions (fail-closed).

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// TTL bounds and capacity for a permitted set.
#[derive(Debug, Clone, Copy)]
pub struct SetConfig {
    pub min_ttl: Duration,
    pub max_ttl: Duration,
    pub max_size: usize,
}

impl Default for SetConfig {
    fn default() -> Self {
        SetConfig {
            min_ttl: Duration::from_secs(60),
            max_ttl: Duration::from_secs(3600),
            max_size: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    refcount: u32,
    /// `None` => never expires (literal IP grant). `Some(deadline)` => host pin.
    deadline: Option<Instant>,
}

/// Outcome of an `allow` attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Added {
    /// IP is now permitted (newly inserted).
    Inserted,
    /// IP was already present; refcount bumped / deadline extended.
    Refreshed,
    /// Rejected: the set is at `max_size` and this IP is new. Fail-closed.
    Full,
}

/// A per-identity set of permitted destination IPs with eviction.
#[derive(Debug)]
pub struct PermitSet {
    cfg: SetConfig,
    entries: HashMap<IpAddr, Entry>,
}

impl PermitSet {
    pub fn new(cfg: SetConfig) -> Self {
        PermitSet {
            cfg,
            entries: HashMap::new(),
        }
    }

    /// Seed a literal `net:<ip>` grant: permitted for the life of the set, never
    /// TTL-evicted. Subject to `max_size`.
    pub fn allow_literal(&mut self, ip: IpAddr) -> Added {
        self.insert(ip, None)
    }

    /// Permit a host-resolved IP for `ttl` (clamped to `[min_ttl, max_ttl]`),
    /// measured from `now`. Re-allowing extends the later deadline and bumps the
    /// refcount.
    pub fn allow_resolved(&mut self, ip: IpAddr, ttl: Duration, now: Instant) -> Added {
        let clamped = ttl.clamp(self.cfg.min_ttl, self.cfg.max_ttl);
        self.insert(ip, Some(now + clamped))
    }

    fn insert(&mut self, ip: IpAddr, deadline: Option<Instant>) -> Added {
        if let Some(e) = self.entries.get_mut(&ip) {
            e.refcount = e.refcount.saturating_add(1);
            // Extend to the later deadline; a literal (None) wins over any TTL.
            e.deadline = match (e.deadline, deadline) {
                (None, _) | (_, None) => None,
                (Some(a), Some(b)) => Some(a.max(b)),
            };
            return Added::Refreshed;
        }
        if self.entries.len() >= self.cfg.max_size {
            return Added::Full;
        }
        self.entries.insert(
            ip,
            Entry {
                refcount: 1,
                deadline,
            },
        );
        Added::Inserted
    }

    /// Drop one reference to `ip`. Removes the entry when the last reference is
    /// gone. Returns true if the entry was removed.
    pub fn revoke(&mut self, ip: IpAddr) -> bool {
        if let Some(e) = self.entries.get_mut(&ip) {
            e.refcount = e.refcount.saturating_sub(1);
            if e.refcount == 0 {
                self.entries.remove(&ip);
                return true;
            }
        }
        false
    }

    /// Remove every entry whose TTL deadline is at or before `now`. Literal
    /// entries (no deadline) are never swept. Returns the evicted IPs so a
    /// backend can mirror the removals.
    pub fn sweep(&mut self, now: Instant) -> Vec<IpAddr> {
        let expired: Vec<IpAddr> = self
            .entries
            .iter()
            .filter(|(_, e)| matches!(e.deadline, Some(d) if d <= now))
            .map(|(ip, _)| *ip)
            .collect();
        for ip in &expired {
            self.entries.remove(ip);
        }
        expired
    }

    /// Whether `ip` is currently permitted (present and not past its deadline).
    pub fn contains(&self, ip: &IpAddr, now: Instant) -> bool {
        match self.entries.get(ip) {
            Some(e) => match e.deadline {
                Some(d) => d > now,
                None => true,
            },
            None => false,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn cfg() -> SetConfig {
        SetConfig {
            min_ttl: Duration::from_secs(10),
            max_ttl: Duration::from_secs(100),
            max_size: 3,
        }
    }

    #[test]
    fn literal_never_expires() {
        let mut s = PermitSet::new(cfg());
        let now = Instant::now();
        assert_eq!(s.allow_literal(ip("1.2.3.4")), Added::Inserted);
        // Far future: still permitted, sweep keeps it.
        let later = now + Duration::from_secs(10_000);
        assert!(s.contains(&ip("1.2.3.4"), later));
        assert!(s.sweep(later).is_empty());
        assert!(s.contains(&ip("1.2.3.4"), later));
    }

    #[test]
    fn resolved_expires_after_ttl() {
        let mut s = PermitSet::new(cfg());
        let now = Instant::now();
        s.allow_resolved(ip("9.9.9.9"), Duration::from_secs(30), now);
        assert!(s.contains(&ip("9.9.9.9"), now + Duration::from_secs(29)));
        // At/after deadline: not permitted, and swept.
        let past = now + Duration::from_secs(31);
        assert!(!s.contains(&ip("9.9.9.9"), past));
        assert_eq!(s.sweep(past), vec![ip("9.9.9.9")]);
        assert!(s.is_empty());
    }

    #[test]
    fn ttl_is_clamped() {
        let mut s = PermitSet::new(cfg());
        let now = Instant::now();
        // Below min (10s) -> floored to 10s.
        s.allow_resolved(ip("1.1.1.1"), Duration::from_secs(1), now);
        assert!(s.contains(&ip("1.1.1.1"), now + Duration::from_secs(9)));
        // Above max (100s) -> capped to 100s.
        s.allow_resolved(ip("2.2.2.2"), Duration::from_secs(9999), now);
        assert!(s.contains(&ip("2.2.2.2"), now + Duration::from_secs(99)));
        assert!(!s.contains(&ip("2.2.2.2"), now + Duration::from_secs(101)));
    }

    #[test]
    fn refcount_holds_until_last_revoke() {
        let mut s = PermitSet::new(cfg());
        let now = Instant::now();
        assert_eq!(s.allow_literal(ip("5.5.5.5")), Added::Inserted);
        assert_eq!(s.allow_literal(ip("5.5.5.5")), Added::Refreshed); // rc=2
        assert!(!s.revoke(ip("5.5.5.5"))); // rc=1, still present
        assert!(s.contains(&ip("5.5.5.5"), now));
        assert!(s.revoke(ip("5.5.5.5"))); // rc=0, removed
        assert!(!s.contains(&ip("5.5.5.5"), now));
    }

    #[test]
    fn reallow_extends_to_later_deadline() {
        let mut s = PermitSet::new(cfg());
        let now = Instant::now();
        s.allow_resolved(ip("3.3.3.3"), Duration::from_secs(20), now);
        // Re-resolve later with a fresh TTL: deadline moves forward.
        s.allow_resolved(
            ip("3.3.3.3"),
            Duration::from_secs(50),
            now + Duration::from_secs(10),
        );
        // Original deadline was now+20; new is now+60. Still alive at now+30.
        assert!(s.contains(&ip("3.3.3.3"), now + Duration::from_secs(30)));
    }

    #[test]
    fn literal_beats_ttl_on_collision() {
        let mut s = PermitSet::new(cfg());
        let now = Instant::now();
        s.allow_resolved(ip("4.4.4.4"), Duration::from_secs(20), now);
        s.allow_literal(ip("4.4.4.4")); // promotes to never-expire
        let far = now + Duration::from_secs(10_000);
        assert!(s.contains(&ip("4.4.4.4"), far));
    }

    #[test]
    fn max_size_is_fail_closed() {
        let mut s = PermitSet::new(cfg()); // max_size = 3
        let now = Instant::now();
        assert_eq!(
            s.allow_resolved(ip("10.0.0.1"), Duration::from_secs(10), now),
            Added::Inserted
        );
        assert_eq!(
            s.allow_resolved(ip("10.0.0.2"), Duration::from_secs(10), now),
            Added::Inserted
        );
        assert_eq!(s.allow_literal(ip("10.0.0.3")), Added::Inserted);
        // Fourth distinct IP is rejected, not silently dropped.
        assert_eq!(
            s.allow_resolved(ip("10.0.0.4"), Duration::from_secs(10), now),
            Added::Full
        );
        assert!(!s.contains(&ip("10.0.0.4"), now));
        // But refreshing an existing IP at capacity is fine.
        assert_eq!(s.allow_literal(ip("10.0.0.1")), Added::Refreshed);
    }

    #[test]
    fn ipv6_supported() {
        let mut s = PermitSet::new(cfg());
        let now = Instant::now();
        s.allow_resolved(ip("2606:4700:4700::1111"), Duration::from_secs(30), now);
        assert!(s.contains(&ip("2606:4700:4700::1111"), now));
    }
}

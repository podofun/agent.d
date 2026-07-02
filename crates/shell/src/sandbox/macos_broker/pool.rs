//! Lease pool of dedicated sandbox uids. Each concurrent sandboxed exec holds
//! one uid for the life of its broker connection; the pool fails closed when
//! exhausted rather than ever sharing a uid (a shared uid would let one child's
//! pf anchor / ACLs leak onto another's traffic).
//!
//! Pure and OS-agnostic so it is unit-tested everywhere.

use std::collections::BTreeSet;
use std::sync::Mutex;

/// One provisioned sandbox identity: a uid and its account name (`_agentd_sbxN`,
/// needed for the `chmod +a "user:<name>"` ACLs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxUser {
    pub uid: u32,
    pub name: String,
}

/// Fixed set of sandbox users, leased one at a time.
pub struct UidPool {
    all: Vec<SandboxUser>,
    free: Mutex<BTreeSet<u32>>,
}

/// A leased uid, returned to the pool on drop (RAII: a dropped/panicked session
/// never strands its uid).
pub struct Lease<'p> {
    pool: &'p UidPool,
    user: SandboxUser,
}

impl<'p> Lease<'p> {
    pub fn user(&self) -> &SandboxUser {
        &self.user
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.pool.free.lock().unwrap().insert(self.user.uid);
    }
}

impl UidPool {
    pub fn new(users: Vec<SandboxUser>) -> Self {
        let free = users.iter().map(|u| u.uid).collect();
        UidPool {
            all: users,
            free: Mutex::new(free),
        }
    }

    /// Lease a free uid, or `None` when every uid is in use (caller maps to a
    /// retryable `pool-exhausted` error — never blocks, never shares).
    pub fn lease(&self) -> Option<Lease<'_>> {
        let uid = {
            let mut free = self.free.lock().unwrap();
            let uid = *free.iter().next()?;
            free.remove(&uid);
            uid
        };
        let user = self.all.iter().find(|u| u.uid == uid).cloned()?;
        Some(Lease { pool: self, user })
    }

    /// All uids the pool manages (for startup anchor sweep).
    pub fn all_uids(&self) -> impl Iterator<Item = u32> + '_ {
        self.all.iter().map(|u| u.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> UidPool {
        UidPool::new(vec![
            SandboxUser { uid: 700, name: "_agentd_sbx0".into() },
            SandboxUser { uid: 701, name: "_agentd_sbx1".into() },
        ])
    }

    #[test]
    fn leases_distinct_uids_then_exhausts() {
        let p = pool();
        let a = p.lease().unwrap();
        let b = p.lease().unwrap();
        assert_ne!(a.user().uid, b.user().uid);
        assert!(p.lease().is_none(), "third lease must fail closed");
    }

    #[test]
    fn drop_returns_uid_to_pool() {
        let p = pool();
        let a = p.lease().unwrap();
        let uid_a = a.user().uid;
        let _b = p.lease().unwrap();
        assert!(p.lease().is_none());
        drop(a);
        let c = p.lease().expect("uid freed on drop is reusable");
        assert_eq!(c.user().uid, uid_a);
    }

    #[test]
    fn lease_carries_account_name() {
        let p = pool();
        let l = p.lease().unwrap();
        assert!(l.user().name.starts_with("_agentd_sbx"));
    }

    #[test]
    fn all_uids_lists_every_uid() {
        let p = pool();
        let mut v: Vec<u32> = p.all_uids().collect();
        v.sort();
        assert_eq!(v, vec![700, 701]);
    }
}

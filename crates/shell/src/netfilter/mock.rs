//! In-memory [`NetFilter`] for unit tests. Backs the permitted set with
//! [`PermitSet`] so the resolver and lifecycle can be exercised without any
//! kernel mechanism or privilege.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Added, FilterError, FilterHandle, NetFilter, PermitSet, SetConfig, Supports};

#[derive(Clone)]
pub struct MockHandle {
    pub set: Arc<Mutex<PermitSet>>,
    pub torn_down: Arc<Mutex<bool>>,
}

impl FilterHandle for MockHandle {}

impl MockHandle {
    pub fn permits(&self, ip: &str) -> bool {
        self.set
            .lock()
            .unwrap()
            .contains(&ip.parse().unwrap(), Instant::now())
    }
    pub fn is_torn_down(&self) -> bool {
        *self.torn_down.lock().unwrap()
    }
}

#[derive(Default)]
pub struct MockFilter {
    pub cfg: SetConfig,
    pub supports: Option<Supports>,
}

impl MockFilter {
    pub fn new() -> Self {
        MockFilter {
            cfg: SetConfig::default(),
            supports: None,
        }
    }
}

impl NetFilter for MockFilter {
    type Handle = MockHandle;

    fn supports(&self) -> Supports {
        self.supports.unwrap_or(Supports {
            ipv4: true,
            ipv6: true,
        })
    }

    fn provision(&self, literal_ips: &[IpAddr]) -> Result<MockHandle, FilterError> {
        let mut set = PermitSet::new(self.cfg);
        for ip in literal_ips {
            if set.allow_literal(*ip) == Added::Full {
                return Err(FilterError::Full);
            }
        }
        Ok(MockHandle {
            set: Arc::new(Mutex::new(set)),
            torn_down: Arc::new(Mutex::new(false)),
        })
    }

    fn commit_allow(
        &self,
        handle: &MockHandle,
        ips: &[IpAddr],
        ttl: Duration,
    ) -> Result<(), FilterError> {
        let mut set = handle.set.lock().unwrap();
        let now = Instant::now();
        for ip in ips {
            if set.allow_resolved(*ip, ttl, now) == Added::Full {
                return Err(FilterError::Full);
            }
        }
        Ok(())
    }

    fn revoke(&self, handle: &MockHandle, ips: &[IpAddr]) -> Result<(), FilterError> {
        let mut set = handle.set.lock().unwrap();
        for ip in ips {
            set.revoke(*ip);
        }
        Ok(())
    }

    fn teardown(&self, handle: MockHandle) {
        *handle.torn_down.lock().unwrap() = true;
    }
}

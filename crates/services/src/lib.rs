//! Services — long-running Lua tasks the daemon spawns at startup.
//!
//! A **service** is a named Lua function that runs forever (or until daemon
//! shutdown). Examples: Discord/Slack gateway connections, IMAP idle loops,
//! cron-style poll loops. Services are how a package author models "open a
//! socket and hold it" — actions are request/response, runners are
//! single-shot AI workers, services are persistent background work.
//!
//! Permission model: each service runs with `ActiveContext.effective_grants`
//! sourced from `[service.<name>].granted` in `grants.toml`. Default-deny
//! still holds; an unlisted service starts with empty grants and any
//! `ctx.*` call from its body will fail until the user adds the
//! relevant slug.
//!
//! Registration shape is Lua-side (see `agentd-scripting`):
//!
//! ```lua
//! agentd.service("discord_gateway", function(ctx)
//!   local token = ctx.secret.get("discord_token")
//!   local ws = ctx.ws.connect("wss://gateway.discord.gg/?v=10")
//!   -- ...identify, then loop on ws:recv() ...
//! end)
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Static description of a service. The handler itself lives in the Lua
/// registry; this struct only carries the metadata the daemon needs to spawn
/// and track it. `handler_key` is an opaque token (a stringified
/// `mlua::RegistryKey` id) so this struct stays serializable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceDef {
    pub name: String,
    /// Optional tool tie — when set, the service may opt into reading the
    /// tool's grants as a base. Authoritative grants still come from
    /// `[service.<name>].granted`; this field is informational.
    #[serde(default)]
    pub tool: Option<String>,
    /// Source location (e.g. file:line) for diagnostics. Set by the Lua host.
    #[serde(default)]
    pub source: Option<String>,
    /// Supervision policy as a string. `"always"` restarts after every exit
    /// (clean or error); `"on_failure"` restarts only on error; anything else
    /// (including `None`) lets the task exit normally. Honored by the
    /// executor's service supervisor.
    #[serde(default)]
    pub restart: Option<String>,
    /// Initial restart backoff in ms. Doubles after each consecutive failure
    /// up to `backoff_max_ms`. `None` falls back to 1000.
    #[serde(default)]
    pub backoff_ms: Option<u64>,
    /// Cap on the exponential backoff in ms. `None` falls back to 60_000.
    #[serde(default)]
    pub backoff_max_ms: Option<u64>,
}

/// Current lifecycle state. Updated by the supervisor task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceState {
    Pending,
    Running,
    Stopped,
    Crashed,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub name: String,
    pub state: ServiceState,
    pub last_error: Option<String>,
}

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("service `{0}` not registered")]
    NotFound(String),
    #[error("service `{name}`: handler missing")]
    NoHandler { name: String },
    #[error("service `{name}` panicked: {reason}")]
    Panic { name: String, reason: String },
}

#[derive(Default, Clone)]
pub struct ServiceRegistry {
    inner: Arc<RwLock<BTreeMap<String, ServiceEntry>>>,
}

#[derive(Debug, Clone)]
struct ServiceEntry {
    def: ServiceDef,
    state: ServiceState,
    last_error: Option<String>,
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, def: ServiceDef) {
        let mut g = self.inner.write().unwrap();
        g.insert(
            def.name.clone(),
            ServiceEntry {
                def,
                state: ServiceState::Pending,
                last_error: None,
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<ServiceDef> {
        let g = self.inner.read().unwrap();
        g.get(name).map(|e| e.def.clone())
    }

    pub fn names(&self) -> Vec<String> {
        let g = self.inner.read().unwrap();
        g.keys().cloned().collect()
    }

    pub fn list(&self) -> Vec<ServiceDef> {
        let g = self.inner.read().unwrap();
        g.values().map(|e| e.def.clone()).collect()
    }

    pub fn status(&self, name: &str) -> Option<ServiceStatus> {
        let g = self.inner.read().unwrap();
        g.get(name).map(|e| ServiceStatus {
            name: e.def.name.clone(),
            state: e.state,
            last_error: e.last_error.clone(),
        })
    }

    pub fn statuses(&self) -> Vec<ServiceStatus> {
        let g = self.inner.read().unwrap();
        g.values()
            .map(|e| ServiceStatus {
                name: e.def.name.clone(),
                state: e.state,
                last_error: e.last_error.clone(),
            })
            .collect()
    }

    pub fn set_state(&self, name: &str, state: ServiceState) {
        let mut g = self.inner.write().unwrap();
        if let Some(e) = g.get_mut(name) {
            e.state = state;
        }
    }

    pub fn set_error(&self, name: &str, err: impl Into<String>) {
        let mut g = self.inner.write().unwrap();
        if let Some(e) = g.get_mut(name) {
            e.last_error = Some(err.into());
            e.state = ServiceState::Crashed;
        }
    }

    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get() {
        let reg = ServiceRegistry::new();
        reg.insert(ServiceDef {
            name: "discord_gateway".into(),
            tool: Some("discord".into()),
            ..Default::default()
        });
        assert!(reg.get("discord_gateway").is_some());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn state_transitions() {
        let reg = ServiceRegistry::new();
        reg.insert(ServiceDef {
            name: "s".into(),
            ..Default::default()
        });
        let s = reg.status("s").unwrap();
        assert_eq!(s.state, ServiceState::Pending);
        reg.set_state("s", ServiceState::Running);
        assert_eq!(reg.status("s").unwrap().state, ServiceState::Running);
        reg.set_error("s", "boom");
        let s = reg.status("s").unwrap();
        assert_eq!(s.state, ServiceState::Crashed);
        assert_eq!(s.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn shared_state_across_clones() {
        let a = ServiceRegistry::new();
        let b = a.clone();
        a.insert(ServiceDef {
            name: "x".into(),
            ..Default::default()
        });
        assert!(b.get("x").is_some());
    }
}

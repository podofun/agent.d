//! Durable chat sessions: an ordered log of `agentd_ai::Message` turns keyed
//! by a daemon-minted uuid. Runners load a session's turns as conversation
//! history, append what the run produced, and periodically compact old turns
//! into a summary.
//!
//! Every session has an **owner**: the interface (or Lua service) whose caller
//! created it, plus that caller's `user` when one was declared. A [`Scope`]
//! built from the current caller decides what it may see; anything outside
//! the scope behaves as if it did not exist.
//!
//! `MemSessionStore` is the in-process test double; `RedbSessionStore` is the
//! production impl and lives in the same redb file as `ctx.memory`.

mod redb_store;
pub use redb_store::RedbSessionStore;

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use agentd_ai::Message;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("{0}")]
    Backend(String),
    #[error("session `{0}` does not exist")]
    NotFound(String),
    #[error("a session labelled `{0}` already exists")]
    LabelTaken(String),
}

pub type Result<T> = std::result::Result<T, SessionError>;

/// Who a caller is, for session visibility. Built from the `Caller` at every
/// entry point (WebSocket, Lua), never from request parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// `interface:<id>` or `service:<name>`.
    pub owner: String,
    /// The caller's declared end-user, if any.
    pub user: Option<String>,
}

impl Scope {
    /// Derive the scope from caller identity fields. An interface wins over a
    /// service; a caller with neither (bare local scripts) gets `local`.
    pub fn from_caller(interface: Option<&str>, service: Option<&str>, user: Option<&str>) -> Self {
        let owner = match (interface, service) {
            (Some(i), _) => format!("interface:{i}"),
            (None, Some(s)) => format!("service:{s}"),
            (None, None) => "local".to_string(),
        };
        Self {
            owner,
            user: user.map(str::to_string),
        }
    }

    /// May this caller see `meta`? Same owner, and when the session was
    /// created for a specific user, the same user.
    pub fn permits(&self, meta: &SessionMeta) -> bool {
        meta.owner == self.owner && (meta.user.is_none() || meta.user == self.user)
    }
}

/// Everything about a session except its turns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: String,
    /// `interface:<id>` or `service:<name>` of the creating caller. Rows
    /// written before ownership existed decode with an empty owner, which
    /// no scope matches, so they are invisible rather than fatal.
    #[serde(default)]
    pub owner: String,
    /// Runner that created the session, informational only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// End-user id supplied by the bridging interface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Caller's external id (e.g. `telegram-42`). Unique per daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
    /// Number of turns currently stored (after compaction).
    pub turn_count: u64,
    /// Next sequence number to assign; never reused after compaction.
    pub next_seq: u64,
    pub compactions: u32,
}

/// Inputs for `create`. `owner` and `user` come from the caller's [`Scope`].
#[derive(Debug, Clone, Default)]
pub struct NewSession {
    pub owner: String,
    pub user: Option<String>,
    /// Caller's external id, unique per owner.
    pub label: Option<String>,
    pub runner: Option<String>,
}

impl NewSession {
    pub fn in_scope(scope: &Scope) -> Self {
        Self {
            owner: scope.owner.clone(),
            user: scope.user.clone(),
            ..Default::default()
        }
    }
    pub fn label(mut self, label: Option<String>) -> Self {
        self.label = label;
        self
    }
    pub fn runner(mut self, runner: Option<String>) -> Self {
        self.runner = runner;
        self
    }
}

/// One compaction: drop the `drop_count` oldest stored turns and append
/// `summary` in their place (it lands at the head of the remaining log).
#[derive(Debug, Clone)]
pub struct Compaction {
    pub drop_count: usize,
    pub summary: Message,
}

pub trait SessionStore: Send + Sync {
    fn create(&self, new: NewSession) -> Result<SessionMeta>;
    /// Raw lookup, no scope check. Callers gate with [`Scope::permits`] or use
    /// [`SessionStore::get_in`].
    fn get(&self, id: &str) -> Result<Option<SessionMeta>>;
    /// Label lookup within one owner.
    fn find_by_label(&self, owner: &str, label: &str) -> Result<Option<SessionMeta>>;
    /// Sessions the scope may see, newest first.
    fn list(&self, scope: &Scope, limit: usize) -> Result<Vec<SessionMeta>>;

    /// `get` filtered by scope: a session outside it reads as absent.
    fn get_in(&self, scope: &Scope, id: &str) -> Result<Option<SessionMeta>> {
        Ok(self.get(id)?.filter(|m| scope.permits(m)))
    }
    /// `find_by_label` within the scope's owner, filtered by its user.
    fn find_in(&self, scope: &Scope, label: &str) -> Result<Option<SessionMeta>> {
        Ok(self
            .find_by_label(&scope.owner, label)?
            .filter(|m| scope.permits(m)))
    }
    /// Turns in conversation order.
    fn turns(&self, id: &str) -> Result<Vec<Message>>;
    /// Append turns atomically. `NotFound` if the session is gone.
    fn append(&self, id: &str, turns: &[Message]) -> Result<SessionMeta>;
    /// Replace the oldest turns with a summary atomically.
    fn compact(&self, id: &str, compaction: Compaction) -> Result<SessionMeta>;
    /// Returns `false` if there was nothing to delete.
    fn delete(&self, id: &str) -> Result<bool>;
    /// Replace the label. `None` clears it. `LabelTaken` if another session of
    /// the same owner already uses it, `NotFound` if the session is gone.
    fn relabel(&self, id: &str, label: Option<String>) -> Result<SessionMeta>;
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn fresh_meta(new: NewSession) -> SessionMeta {
    let now = now_secs();
    SessionMeta {
        id: new_id(),
        owner: new.owner,
        runner: new.runner,
        user: new.user,
        label: new.label,
        created_at: now,
        updated_at: now,
        turn_count: 0,
        next_seq: 0,
        compactions: 0,
    }
}

/// In-process store for tests. Not durable.
#[derive(Default)]
pub struct MemSessionStore {
    inner: RwLock<MemInner>,
}

#[derive(Default)]
struct MemInner {
    metas: BTreeMap<String, SessionMeta>,
    turns: BTreeMap<String, Vec<Message>>,
}

impl MemSessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for MemSessionStore {
    fn create(&self, new: NewSession) -> Result<SessionMeta> {
        let mut g = self.inner.write().unwrap();
        if let Some(label) = new.label.as_deref()
            && g.metas
                .values()
                .any(|m| m.owner == new.owner && m.label.as_deref() == Some(label))
        {
            return Err(SessionError::LabelTaken(label.to_string()));
        }
        let meta = fresh_meta(new);
        g.metas.insert(meta.id.clone(), meta.clone());
        g.turns.insert(meta.id.clone(), Vec::new());
        Ok(meta)
    }
    fn get(&self, id: &str) -> Result<Option<SessionMeta>> {
        Ok(self.inner.read().unwrap().metas.get(id).cloned())
    }
    fn find_by_label(&self, owner: &str, label: &str) -> Result<Option<SessionMeta>> {
        Ok(self
            .inner
            .read()
            .unwrap()
            .metas
            .values()
            .find(|m| m.owner == owner && m.label.as_deref() == Some(label))
            .cloned())
    }
    fn list(&self, scope: &Scope, limit: usize) -> Result<Vec<SessionMeta>> {
        let g = self.inner.read().unwrap();
        let mut v: Vec<SessionMeta> = g
            .metas
            .values()
            .filter(|m| scope.permits(m))
            .cloned()
            .collect();
        v.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        v.truncate(limit);
        Ok(v)
    }
    fn turns(&self, id: &str) -> Result<Vec<Message>> {
        let g = self.inner.read().unwrap();
        g.turns
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))
    }
    fn append(&self, id: &str, turns: &[Message]) -> Result<SessionMeta> {
        let mut g = self.inner.write().unwrap();
        let meta = g
            .metas
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        meta.turn_count += turns.len() as u64;
        meta.next_seq += turns.len() as u64;
        meta.updated_at = now_secs();
        let meta = meta.clone();
        g.turns.get_mut(id).unwrap().extend_from_slice(turns);
        Ok(meta)
    }
    fn compact(&self, id: &str, c: Compaction) -> Result<SessionMeta> {
        let mut g = self.inner.write().unwrap();
        let log = g
            .turns
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        let drop = c.drop_count.min(log.len());
        log.drain(..drop);
        log.insert(0, c.summary);
        let len = log.len() as u64;
        let meta = g.metas.get_mut(id).unwrap();
        meta.turn_count = len;
        meta.next_seq += 1;
        meta.compactions += 1;
        meta.updated_at = now_secs();
        Ok(meta.clone())
    }
    fn delete(&self, id: &str) -> Result<bool> {
        let mut g = self.inner.write().unwrap();
        g.turns.remove(id);
        Ok(g.metas.remove(id).is_some())
    }
    fn relabel(&self, id: &str, label: Option<String>) -> Result<SessionMeta> {
        let mut g = self.inner.write().unwrap();
        let owner = g
            .metas
            .get(id)
            .map(|m| m.owner.clone())
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        if let Some(label) = label.as_deref()
            && g.metas
                .values()
                .any(|m| m.id != id && m.owner == owner && m.label.as_deref() == Some(label))
        {
            return Err(SessionError::LabelTaken(label.to_string()));
        }
        let meta = g.metas.get_mut(id).unwrap();
        meta.label = label;
        meta.updated_at = now_secs();
        Ok(meta.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(owner: &str, user: Option<&str>) -> Scope {
        Scope {
            owner: owner.into(),
            user: user.map(str::to_string),
        }
    }

    #[test]
    fn scope_from_caller_prefers_interface_then_service() {
        assert_eq!(
            Scope::from_caller(Some("webapp"), Some("svc"), None).owner,
            "interface:webapp"
        );
        assert_eq!(
            Scope::from_caller(None, Some("svc"), None).owner,
            "service:svc"
        );
        assert_eq!(Scope::from_caller(None, None, Some("u")).owner, "local");
    }

    #[test]
    fn scope_permits_same_owner_and_matching_or_unset_user() {
        let s = MemSessionStore::new();
        let shared = s
            .create(NewSession::in_scope(&scope("interface:web", None)))
            .unwrap();
        let alice = s
            .create(NewSession::in_scope(&scope("interface:web", Some("alice"))))
            .unwrap();
        let web_anon = scope("interface:web", None);
        let web_alice = scope("interface:web", Some("alice"));
        let web_bob = scope("interface:web", Some("bob"));
        let tg_alice = scope("interface:telegram", Some("alice"));
        assert!(
            web_anon.permits(&shared) && web_alice.permits(&shared) && web_bob.permits(&shared)
        );
        assert!(web_alice.permits(&alice));
        assert!(!web_bob.permits(&alice));
        assert!(!web_anon.permits(&alice));
        assert!(!tg_alice.permits(&alice) && !tg_alice.permits(&shared));
        assert_eq!(s.list(&web_bob, 10).unwrap().len(), 1);
        assert_eq!(s.list(&web_alice, 10).unwrap().len(), 2);
        assert_eq!(s.list(&tg_alice, 10).unwrap().len(), 0);
        assert!(s.get_in(&web_bob, &alice.id).unwrap().is_none());
        assert!(s.get_in(&web_alice, &alice.id).unwrap().is_some());
    }

    #[test]
    fn labels_are_unique_per_owner_only() {
        let s = MemSessionStore::new();
        let web = scope("interface:web", None);
        let tg = scope("interface:telegram", None);
        let a = s
            .create(NewSession::in_scope(&web).label(Some("chat-1".into())))
            .unwrap();
        let b = s
            .create(NewSession::in_scope(&tg).label(Some("chat-1".into())))
            .unwrap();
        assert_ne!(a.id, b.id);
        assert!(matches!(
            s.create(NewSession::in_scope(&web).label(Some("chat-1".into()))),
            Err(SessionError::LabelTaken(_))
        ));
        assert_eq!(s.find_in(&web, "chat-1").unwrap().unwrap().id, a.id);
        assert_eq!(s.find_in(&tg, "chat-1").unwrap().unwrap().id, b.id);
        // A user-bound session is invisible to a label lookup by another user.
        let bob = scope("interface:web", Some("bob"));
        s.create(NewSession::in_scope(&bob).label(Some("mine".into())))
            .unwrap();
        assert!(
            s.find_in(&scope("interface:web", Some("eve")), "mine")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mem_store_round_trip() {
        let s = MemSessionStore::new();
        let sc = scope("interface:ws", None);
        let m = s
            .create(NewSession::in_scope(&sc).label(Some("tg-1".into())))
            .unwrap();
        assert_eq!(s.find_in(&sc, "tg-1").unwrap().unwrap().id, m.id);
        s.append(&m.id, &[Message::user("hi"), Message::assistant("yo")])
            .unwrap();
        assert_eq!(s.turns(&m.id).unwrap().len(), 2);
        let meta = s
            .compact(
                &m.id,
                Compaction {
                    drop_count: 1,
                    summary: Message::user("[summary]"),
                },
            )
            .unwrap();
        assert_eq!(meta.turn_count, 2);
        assert_eq!(meta.compactions, 1);
        let t = s.turns(&m.id).unwrap();
        assert_eq!(t[0].content, "[summary]");
        assert_eq!(t[1].content, "yo");
        assert!(s.delete(&m.id).unwrap());
        assert!(!s.delete(&m.id).unwrap());
        assert!(matches!(s.turns(&m.id), Err(SessionError::NotFound(_))));
    }

    #[test]
    fn relabel_replaces_the_label_and_keeps_it_unique_per_owner() {
        let s = MemSessionStore::new();
        let web = scope("interface:web", None);
        let a = s.create(NewSession::in_scope(&web)).unwrap();
        let b = s
            .create(NewSession::in_scope(&web).label(Some("taken".into())))
            .unwrap();
        let renamed = s.relabel(&a.id, Some("work".into())).unwrap();
        assert_eq!(renamed.label.as_deref(), Some("work"));
        assert_eq!(s.find_by_label("interface:web", "work").unwrap().unwrap().id, a.id);
        assert!(matches!(
            s.relabel(&a.id, Some("taken".into())),
            Err(SessionError::LabelTaken(_))
        ));
        assert!(matches!(s.relabel("missing", None), Err(SessionError::NotFound(_))));
        s.relabel(&b.id, None).unwrap();
        assert!(s.find_by_label("interface:web", "taken").unwrap().is_none());
    }
}

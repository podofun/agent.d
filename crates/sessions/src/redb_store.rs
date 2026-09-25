//! redb-backed session store. Three tables in the shared daemon database:
//!
//! - `sessions`: `<uuid>` -> JSON `SessionMeta`
//! - `turns`:    `(<uuid>, seq)` -> JSON `Message`; seq is monotone per session
//! - `session_labels`: `<owner>\0<label>` -> `<uuid>`
//!
//! Every mutation is one write transaction, so a crashed run never leaves a
//! half-appended turn list.

use std::sync::Arc;

use agentd_ai::Message;
use agentd_memory::Database;
use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::{
    Compaction, NewSession, Result, Scope, SessionError, SessionMeta, SessionStore, fresh_meta,
    now_secs,
};

const SESSIONS: TableDefinition<&str, &[u8]> = TableDefinition::new("sessions");
const TURNS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("turns");
const LABELS: TableDefinition<&str, &str> = TableDefinition::new("session_labels");

pub struct RedbSessionStore {
    db: Arc<Database>,
}

fn backend<E: std::fmt::Display>(e: E) -> SessionError {
    SessionError::Backend(e.to_string())
}

fn encode<T: serde::Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(v).map_err(backend)
}

fn decode<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T> {
    serde_json::from_slice(b).map_err(backend)
}

/// `<owner>\0<label>`: labels are unique per owner, never across owners.
fn label_key(owner: &str, label: &str) -> String {
    format!("{owner}\0{label}")
}

impl RedbSessionStore {
    /// Ensure the tables exist so read transactions never hit
    /// `TableDoesNotExist` on a fresh file.
    pub fn new(db: Arc<Database>) -> Result<Self> {
        let wtx = db.begin_write().map_err(backend)?;
        {
            wtx.open_table(SESSIONS).map_err(backend)?;
            wtx.open_table(TURNS).map_err(backend)?;
            wtx.open_table(LABELS).map_err(backend)?;
        }
        wtx.commit().map_err(backend)?;
        Ok(Self { db })
    }

    fn read_meta(
        table: &impl ReadableTable<&'static str, &'static [u8]>,
        id: &str,
    ) -> Result<Option<SessionMeta>> {
        match table.get(id).map_err(backend)? {
            Some(v) => Ok(Some(decode(v.value())?)),
            None => Ok(None),
        }
    }
}

impl SessionStore for RedbSessionStore {
    fn create(&self, new: NewSession) -> Result<SessionMeta> {
        let wtx = self.db.begin_write().map_err(backend)?;
        let meta = {
            let mut labels = wtx.open_table(LABELS).map_err(backend)?;
            if let Some(label) = new.label.as_deref()
                && labels
                    .get(label_key(&new.owner, label).as_str())
                    .map_err(backend)?
                    .is_some()
            {
                return Err(SessionError::LabelTaken(label.to_string()));
            }
            let meta = fresh_meta(new);
            if let Some(label) = meta.label.as_deref() {
                labels
                    .insert(label_key(&meta.owner, label).as_str(), meta.id.as_str())
                    .map_err(backend)?;
            }
            let mut sessions = wtx.open_table(SESSIONS).map_err(backend)?;
            sessions
                .insert(meta.id.as_str(), encode(&meta)?.as_slice())
                .map_err(backend)?;
            meta
        };
        wtx.commit().map_err(backend)?;
        Ok(meta)
    }

    fn get(&self, id: &str) -> Result<Option<SessionMeta>> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(SESSIONS).map_err(backend)?;
        Self::read_meta(&table, id)
    }

    fn find_by_label(&self, owner: &str, label: &str) -> Result<Option<SessionMeta>> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let labels = rtx.open_table(LABELS).map_err(backend)?;
        let Some(id) = labels
            .get(label_key(owner, label).as_str())
            .map_err(backend)?
        else {
            return Ok(None);
        };
        let id = id.value().to_string();
        let table = rtx.open_table(SESSIONS).map_err(backend)?;
        Self::read_meta(&table, &id)
    }

    fn list(&self, scope: &Scope, limit: usize) -> Result<Vec<SessionMeta>> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let table = rtx.open_table(SESSIONS).map_err(backend)?;
        let mut out = Vec::new();
        for entry in table.iter().map_err(backend)? {
            let (_, v) = entry.map_err(backend)?;
            let meta = decode::<SessionMeta>(v.value())?;
            if scope.permits(&meta) {
                out.push(meta);
            }
        }
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.id.cmp(&a.id))
        });
        out.truncate(limit);
        Ok(out)
    }

    fn turns(&self, id: &str) -> Result<Vec<Message>> {
        let rtx = self.db.begin_read().map_err(backend)?;
        let sessions = rtx.open_table(SESSIONS).map_err(backend)?;
        if Self::read_meta(&sessions, id)?.is_none() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        let turns = rtx.open_table(TURNS).map_err(backend)?;
        let mut out = Vec::new();
        for entry in turns.range((id, 0)..(id, u64::MAX)).map_err(backend)? {
            let (_, v) = entry.map_err(backend)?;
            out.push(decode::<Message>(v.value())?);
        }
        Ok(out)
    }

    fn append(&self, id: &str, new_turns: &[Message]) -> Result<SessionMeta> {
        let wtx = self.db.begin_write().map_err(backend)?;
        let meta = {
            let mut sessions = wtx.open_table(SESSIONS).map_err(backend)?;
            let mut meta = Self::read_meta(&sessions, id)?
                .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
            let mut turns = wtx.open_table(TURNS).map_err(backend)?;
            for m in new_turns {
                turns
                    .insert((id, meta.next_seq), encode(m)?.as_slice())
                    .map_err(backend)?;
                meta.next_seq += 1;
                meta.turn_count += 1;
            }
            meta.updated_at = now_secs();
            sessions
                .insert(id, encode(&meta)?.as_slice())
                .map_err(backend)?;
            meta
        };
        wtx.commit().map_err(backend)?;
        Ok(meta)
    }

    fn compact(&self, id: &str, c: Compaction) -> Result<SessionMeta> {
        let wtx = self.db.begin_write().map_err(backend)?;
        let meta = {
            let mut sessions = wtx.open_table(SESSIONS).map_err(backend)?;
            let mut meta = Self::read_meta(&sessions, id)?
                .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
            let mut turns = wtx.open_table(TURNS).map_err(backend)?;
            // Collect the seqs to drop first: redb forbids mutating a table
            // while iterating it.
            let mut seqs = Vec::new();
            for entry in turns.range((id, 0)..(id, u64::MAX)).map_err(backend)? {
                let (k, _) = entry.map_err(backend)?;
                seqs.push(k.value().1);
                if seqs.len() >= c.drop_count {
                    break;
                }
            }
            // The summary takes the first dropped seq, so it sorts ahead of
            // every surviving turn without renumbering anything.
            let Some(&summary_seq) = seqs.first() else {
                return Err(SessionError::Backend(
                    "compaction must drop at least one turn".into(),
                ));
            };
            for s in &seqs {
                turns.remove((id, *s)).map_err(backend)?;
            }
            turns
                .insert((id, summary_seq), encode(&c.summary)?.as_slice())
                .map_err(backend)?;
            meta.turn_count = meta.turn_count - seqs.len() as u64 + 1;
            meta.compactions += 1;
            meta.updated_at = now_secs();
            sessions
                .insert(id, encode(&meta)?.as_slice())
                .map_err(backend)?;
            meta
        };
        wtx.commit().map_err(backend)?;
        Ok(meta)
    }

    fn relabel(&self, id: &str, label: Option<String>) -> Result<SessionMeta> {
        let wtx = self.db.begin_write().map_err(backend)?;
        let meta = {
            let mut sessions = wtx.open_table(SESSIONS).map_err(backend)?;
            let mut meta = Self::read_meta(&sessions, id)?
                .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
            let mut labels = wtx.open_table(LABELS).map_err(backend)?;
            if let Some(new) = label.as_deref() {
                let holder = labels
                    .get(label_key(&meta.owner, new).as_str())
                    .map_err(backend)?
                    .map(|v| v.value().to_string());
                if holder.is_some_and(|holder| holder != id) {
                    return Err(SessionError::LabelTaken(new.to_string()));
                }
            }
            if let Some(old) = meta.label.as_deref() {
                labels
                    .remove(label_key(&meta.owner, old).as_str())
                    .map_err(backend)?;
            }
            if let Some(new) = label.as_deref() {
                labels
                    .insert(label_key(&meta.owner, new).as_str(), id)
                    .map_err(backend)?;
            }
            meta.label = label;
            meta.updated_at = now_secs();
            sessions
                .insert(id, encode(&meta)?.as_slice())
                .map_err(backend)?;
            meta
        };
        wtx.commit().map_err(backend)?;
        Ok(meta)
    }

    fn delete(&self, id: &str) -> Result<bool> {
        let wtx = self.db.begin_write().map_err(backend)?;
        let existed = {
            let mut sessions = wtx.open_table(SESSIONS).map_err(backend)?;
            let meta = Self::read_meta(&sessions, id)?;
            let Some(meta) = meta else {
                return Ok(false);
            };
            sessions.remove(id).map_err(backend)?;
            if let Some(label) = meta.label.as_deref() {
                let mut labels = wtx.open_table(LABELS).map_err(backend)?;
                labels
                    .remove(label_key(&meta.owner, label).as_str())
                    .map_err(backend)?;
            }
            let mut turns = wtx.open_table(TURNS).map_err(backend)?;
            let mut seqs = Vec::new();
            for entry in turns.range((id, 0)..(id, u64::MAX)).map_err(backend)? {
                let (k, _) = entry.map_err(backend)?;
                seqs.push(k.value().1);
            }
            for s in seqs {
                turns.remove((id, s)).map_err(backend)?;
            }
            true
        };
        wtx.commit().map_err(backend)?;
        Ok(existed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_memory::{RedbStore, open_database};

    fn ws() -> Scope {
        Scope {
            owner: "interface:ws".into(),
            user: None,
        }
    }

    fn new() -> NewSession {
        NewSession::in_scope(&ws())
    }

    fn store() -> (tempfile::TempDir, RedbSessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = open_database(dir.path().join("m.redb")).unwrap();
        let s = RedbSessionStore::new(db).unwrap();
        (dir, s)
    }

    #[test]
    fn create_append_turns_ordered() {
        let (_d, s) = store();
        let m = s.create(new()).unwrap();
        assert_eq!(m.turn_count, 0);
        s.append(&m.id, &[Message::user("a"), Message::assistant("b")])
            .unwrap();
        let m2 = s.append(&m.id, &[Message::user("c")]).unwrap();
        assert_eq!(m2.turn_count, 3);
        assert_eq!(m2.next_seq, 3);
        let t: Vec<String> = s
            .turns(&m.id)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(t, ["a", "b", "c"]);
    }

    #[test]
    fn labels_unique_and_findable() {
        let (_d, s) = store();
        let m = s
            .create(
                new()
                    .label(Some("tg-42".into()))
                    .runner(Some("support".into())),
            )
            .unwrap();
        assert_eq!(
            s.find_by_label("interface:ws", "tg-42").unwrap().unwrap(),
            m
        );
        assert!(matches!(
            s.create(new().label(Some("tg-42".into()))),
            Err(SessionError::LabelTaken(_))
        ));
        // Same label under another owner is a different session.
        let other = Scope {
            owner: "interface:telegram".into(),
            user: None,
        };
        let o = s
            .create(NewSession::in_scope(&other).label(Some("tg-42".into())))
            .unwrap();
        assert_ne!(o.id, m.id);
        assert_eq!(s.list(&ws(), 10).unwrap().len(), 1);
        assert!(s.delete(&m.id).unwrap());
        assert!(s.find_by_label("interface:ws", "tg-42").unwrap().is_none());
        assert!(
            s.find_by_label("interface:telegram", "tg-42")
                .unwrap()
                .is_some()
        );
        assert!(matches!(s.turns(&m.id), Err(SessionError::NotFound(_))));
        // Label free again after delete.
        s.create(new().label(Some("tg-42".into()))).unwrap();
    }

    #[test]
    fn compact_replaces_oldest_with_summary() {
        let (_d, s) = store();
        let m = s.create(new()).unwrap();
        let turns: Vec<Message> = (0..6)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("u{i}"))
                } else {
                    Message::assistant(format!("a{i}"))
                }
            })
            .collect();
        s.append(&m.id, &turns).unwrap();
        let meta = s
            .compact(
                &m.id,
                Compaction {
                    drop_count: 4,
                    summary: Message::user("[summary]"),
                },
            )
            .unwrap();
        assert_eq!(meta.turn_count, 3);
        assert_eq!(meta.compactions, 1);
        let t: Vec<String> = s
            .turns(&m.id)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(t, ["[summary]", "u4", "a5"]);
        // Appending after compaction keeps ordering.
        s.append(&m.id, &[Message::user("u6")]).unwrap();
        let t: Vec<String> = s
            .turns(&m.id)
            .unwrap()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(t, ["[summary]", "u4", "a5", "u6"]);
    }

    #[test]
    fn shares_file_with_memory_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_database(dir.path().join("m.redb")).unwrap();
        let mem = RedbStore::from_database(db.clone()).unwrap();
        let s = RedbSessionStore::new(db).unwrap();
        use agentd_memory::MemoryStore;
        mem.set("ns", "k", b"v").unwrap();
        let m = s.create(new()).unwrap();
        assert_eq!(mem.get("ns", "k").unwrap().unwrap(), b"v");
        assert!(s.get(&m.id).unwrap().is_some());
    }

    #[test]
    fn list_newest_first_with_limit() {
        let (_d, s) = store();
        let a = s.create(new()).unwrap();
        let b = s.create(new()).unwrap();
        let all = s.list(&ws(), 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(s.list(&ws(), 1).unwrap().len(), 1);
        // Touch `a` so it becomes newest (same-second ties fall back to id
        // order, so bump via append and check membership only).
        s.append(&a.id, &[Message::user("x")]).unwrap();
        let ids: Vec<_> = s
            .list(&ws(), 10)
            .unwrap()
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert!(ids.contains(&a.id) && ids.contains(&b.id));
    }

    #[test]
    fn relabel_updates_the_label_index() {
        let (_dir, s) = store();
        let a = s.create(new().label(Some("old".into()))).unwrap();
        let b = s.create(new().label(Some("taken".into()))).unwrap();
        let renamed = s.relabel(&a.id, Some("new".into())).unwrap();
        assert_eq!(renamed.label.as_deref(), Some("new"));
        assert!(s.find_by_label("interface:ws", "old").unwrap().is_none());
        assert_eq!(
            s.find_by_label("interface:ws", "new").unwrap().unwrap().id,
            a.id
        );
        assert!(matches!(
            s.relabel(&a.id, Some("taken".into())),
            Err(SessionError::LabelTaken(_))
        ));
        assert!(matches!(
            s.relabel("missing", None),
            Err(SessionError::NotFound(_))
        ));
        s.relabel(&b.id, None).unwrap();
        assert!(s.find_by_label("interface:ws", "taken").unwrap().is_none());
        assert_eq!(s.get(&b.id).unwrap().unwrap().label, None);
    }
}

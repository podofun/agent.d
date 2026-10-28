use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::{EntryDecoration, NodeId, Op, Props, Tree, parts};

/// A chat's UI on the daemon: the tree plus the changes not yet sent.
/// Every change is applied to the tree and recorded as an op, so a frame
/// carries exactly what changed.
#[derive(Debug)]
pub struct Document {
    tree: Tree,
    next: NodeId,
    pending: Vec<Op>,
    reset: bool,
    /// While set, changes go here instead of `pending`: frames rendered
    /// ahead of time are sent on their own.
    capture: Option<Vec<Op>>,
    /// While capturing, the ops that put back what the captured frames
    /// changed, so the document keeps holding the present.
    undo: Vec<Op>,
    /// Named views and the root each one shows.
    views: BTreeMap<String, NodeId>,
    view: String,
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

impl Document {
    pub fn new() -> Self {
        let mut views = BTreeMap::new();
        views.insert("chat".to_string(), parts::CHAT);
        Self {
            tree: Tree::default_chat(),
            next: parts::FIRST_FREE,
            pending: Vec::new(),
            reset: true,
            capture: None,
            undo: Vec::new(),
            views,
            view: "chat".into(),
        }
    }

    /// Back to the default `chat` view. The next frame rebuilds the client's
    /// copy from scratch.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    fn record(&mut self, op: Op) {
        if self.capture.is_some()
            && let Some(back) = self.inverse(&op)
        {
            self.undo.push(back);
        }
        self.tree.apply(&op);
        match self.capture.as_mut() {
            Some(captured) => captured.push(op),
            // Before the first frame only notes wait: the snapshot carries
            // everything else.
            None if !self.reset || matches!(op, Op::Note { .. }) => self.pending.push(op),
            None => {}
        }
    }

    /// The changes since the last call, or with `true` the whole UI when
    /// the client's copy must be rebuilt.
    pub fn take_pending(&mut self) -> (bool, Vec<Op>) {
        if std::mem::take(&mut self.reset) {
            let mut ops = self.tree.snapshot();
            ops.extend(
                self.pending
                    .drain(..)
                    .filter(|op| matches!(op, Op::Note { .. })),
            );
            return (true, ops);
        }
        (false, std::mem::take(&mut self.pending))
    }

    pub fn capture_begin(&mut self) {
        self.capture = Some(Vec::new());
    }

    /// The captured frames' ops. Frames are drawn ahead of time, so their
    /// changes are taken back out of the document: it keeps the present.
    pub fn capture_end(&mut self) -> Vec<Op> {
        for op in std::mem::take(&mut self.undo).into_iter().rev() {
            self.tree.apply(&op);
        }
        self.capture.take().unwrap_or_default()
    }

    /// The op that undoes setting a property or a style value.
    fn inverse(&self, op: &Op) -> Option<Op> {
        let (node, style, key) = match op {
            Op::Set { node, key, .. } => (*node, false, key),
            Op::Style { node, key, .. } => (*node, true, key),
            _ => return None,
        };
        let n = self.tree.get(node)?;
        let map = if style { &n.style } else { &n.props };
        let value = map.get(key).cloned().unwrap_or(Value::Null);
        Some(if style {
            Op::Style {
                node,
                key: key.clone(),
                value,
            }
        } else {
            Op::Set {
                node,
                key: key.clone(),
                value,
            }
        })
    }

    /// Ops that set each of `keys` back to its current value, for frames
    /// sent ahead that an event cancelled.
    pub fn current(&self, keys: &BTreeSet<(NodeId, bool, String)>) -> Vec<Op> {
        keys.iter()
            .filter_map(|(node, style, key)| {
                let n = self.tree.get(*node)?;
                let map = if *style { &n.style } else { &n.props };
                let value = map.get(key).cloned().unwrap_or(Value::Null);
                Some(if *style {
                    Op::Style {
                        node: *node,
                        key: key.clone(),
                        value,
                    }
                } else {
                    Op::Set {
                        node: *node,
                        key: key.clone(),
                        value,
                    }
                })
            })
            .collect()
    }

    pub fn create(&mut self, kind: &str, props: Props, style: Props) -> NodeId {
        let node = self.next;
        self.next += 1;
        self.record(Op::Create {
            node,
            kind: kind.to_string(),
            props,
            style,
        });
        node
    }

    pub fn exists(&self, node: NodeId) -> bool {
        self.tree.get(node).is_some()
    }

    pub fn kind(&self, node: NodeId) -> Option<&str> {
        self.tree.get(node).map(|n| n.kind.as_str())
    }

    pub fn prop(&self, node: NodeId, key: &str) -> Option<&Value> {
        self.tree.get(node)?.props.get(key)
    }

    pub fn style(&self, node: NodeId, key: &str) -> Option<&Value> {
        self.tree.get(node)?.style.get(key)
    }

    pub fn set(&mut self, node: NodeId, key: &str, value: Value) {
        if self.prop(node, key) == Some(&value)
            || (value.is_null() && self.prop(node, key).is_none())
        {
            return;
        }
        self.record(Op::Set {
            node,
            key: key.to_string(),
            value,
        });
    }

    /// Take a value the client already shows, such as typed text, without
    /// sending it back.
    pub fn sync(&mut self, node: NodeId, key: &str, value: Value) {
        self.tree.apply(&Op::Set {
            node,
            key: key.to_string(),
            value,
        });
    }

    pub fn set_style(&mut self, node: NodeId, key: &str, value: Value) {
        if self.style(node, key) == Some(&value)
            || (value.is_null() && self.style(node, key).is_none())
        {
            return;
        }
        self.record(Op::Style {
            node,
            key: key.to_string(),
            value,
        });
    }

    pub fn children(&self, node: NodeId) -> Vec<NodeId> {
        self.tree
            .get(node)
            .map(|n| n.children.clone())
            .unwrap_or_default()
    }

    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.tree.get(node)?.parent
    }

    /// Put `node` under `parent` at `index` (the end when past it).
    pub fn insert(&mut self, parent: NodeId, node: NodeId, index: usize) -> Result<(), String> {
        if !self.exists(parent) || !self.exists(node) {
            return Err("that element no longer exists".into());
        }
        if self.tree.is_within(parent, node) {
            return Err("an element cannot be put inside itself".into());
        }
        self.record(Op::Insert {
            parent,
            node,
            index,
        });
        Ok(())
    }

    pub fn remove(&mut self, node: NodeId) {
        if self.parent(node).is_some() {
            self.record(Op::Remove { node });
        }
    }

    pub fn destroy(&mut self, node: NodeId) {
        if self.exists(node) && node >= parts::FIRST_FREE {
            self.record(Op::Destroy { node });
        }
    }

    /// The first element with property `id`, searching under the view roots
    /// and then everything else.
    pub fn find(&self, id: &str) -> Option<NodeId> {
        self.tree.find(id)
    }

    /// The first element with property `id` under `within`.
    pub fn find_within(&self, within: NodeId, id: &str) -> Option<NodeId> {
        let n = self.tree.get(within)?;
        for child in &n.children {
            if self.prop(*child, "id").and_then(Value::as_str) == Some(id) {
                return Some(*child);
            }
            if let Some(found) = self.find_within(*child, id) {
                return Some(found);
            }
        }
        None
    }

    pub fn root(&self) -> Option<NodeId> {
        self.tree.root
    }

    pub fn view(&self) -> &str {
        &self.view
    }

    pub fn view_root(&self, name: &str) -> Option<NodeId> {
        self.views.get(name).copied()
    }

    pub fn add_view(&mut self, name: &str, root: NodeId) {
        self.views.insert(name.to_string(), root);
    }

    /// Show the view named `name`, which must have been added.
    pub fn show(&mut self, name: &str) -> Result<(), String> {
        let Some(root) = self.view_root(name) else {
            return Err(format!("there is no view named `{name}`"));
        };
        self.view = name.to_string();
        if self.tree.root != Some(root) {
            self.record(Op::Root { node: root });
        }
        Ok(())
    }

    pub fn bindings(&self) -> &[String] {
        &self.tree.bindings
    }

    pub fn set_bindings(&mut self, keys: Vec<String>) {
        if self.tree.bindings != keys {
            self.record(Op::Bindings { keys });
        }
    }

    pub fn decoration(&self, key: &str) -> EntryDecoration {
        self.tree.entries.get(key).cloned().unwrap_or_default()
    }

    pub fn decorate(&mut self, key: &str, decoration: EntryDecoration) {
        if self.decoration(key) != decoration {
            self.record(Op::Entry {
                key: key.to_string(),
                decoration,
            });
        }
    }

    /// Forget every entry decoration, when the chat switches sessions.
    pub fn clear_entries(&mut self) {
        let keys: Vec<String> = self.tree.entries.keys().cloned().collect();
        for key in keys {
            self.record(Op::Entry {
                key,
                decoration: EntryDecoration::default(),
            });
        }
    }

    /// A note for the user.
    pub fn note(&mut self, text: &str) {
        self.record(Op::Note {
            text: crate::sanitize(text),
            error: false,
        });
    }

    /// A failure to show the user, such as a script error.
    pub fn error(&mut self, text: &str) {
        self.record(Op::Note {
            text: crate::sanitize(text),
            error: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_first_frame_is_a_snapshot_and_later_ones_carry_only_changes() {
        let mut doc = Document::new();
        let (reset, ops) = doc.take_pending();
        assert!(reset && ops.iter().any(|op| matches!(op, Op::Root { .. })));
        let text = doc.create("text", Props::new(), Props::new());
        doc.insert(parts::MAIN, text, 9).unwrap();
        doc.set(text, "text", json!("hi"));
        doc.set(text, "text", json!("hi"));
        let (reset, ops) = doc.take_pending();
        assert!(!reset);
        assert_eq!(ops.len(), 3, "{ops:?}");
        assert!(doc.insert(text, parts::MAIN, 0).is_err());
    }

    #[test]
    fn frames_drawn_ahead_leave_the_present_in_the_document() {
        let mut doc = Document::new();
        doc.take_pending();
        let text = doc.create("text", Props::new(), Props::new());
        doc.insert(parts::MAIN, text, 0).unwrap();
        doc.set(text, "text", json!("now"));
        doc.take_pending();
        doc.capture_begin();
        doc.set(text, "text", json!("later"));
        let ops = doc.capture_end();
        assert_eq!(ops.len(), 1, "the frame still carries its change");
        let current = doc.current(&[(text, false, "text".to_owned())].into_iter().collect());
        assert!(
            matches!(&current[0], Op::Set { value, .. } if value == "now"),
            "{current:?}"
        );
    }
}

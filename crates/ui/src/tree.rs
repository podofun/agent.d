use std::collections::HashMap;

use serde_json::{Value, json};

use crate::{EntryDecoration, NodeId, Op, Props, Screen, parts};

/// One element: what it is, its properties and styles, and its children.
#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub kind: String,
    pub props: Props,
    pub style: Props,
    pub children: Vec<NodeId>,
    pub parent: Option<NodeId>,
}

/// A chat's UI as both sides hold it: the daemon records changes against
/// it, and a client applies the same ops to its copy.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Tree {
    pub nodes: HashMap<NodeId, Node>,
    pub root: Option<NodeId>,
    pub bindings: Vec<String>,
    pub entries: HashMap<String, EntryDecoration>,
    pub screens: HashMap<NodeId, Screen>,
}

impl Tree {
    /// The `chat` view as chat draws it today: a header with an empty row
    /// at its right, the conversation in a row named `main`, the prompt and
    /// the status line.
    pub fn default_chat() -> Self {
        let mut tree = Tree::default();
        let parts = [
            (parts::CHAT, "box", "chat", json!({ "direction": "column" })),
            (parts::HEADER, "chat.header", "header", json!({})),
            (
                parts::HEADER_RIGHT,
                "box",
                "header.right",
                json!({ "direction": "row", "gap": 1 }),
            ),
            (
                parts::MAIN,
                "box",
                "main",
                json!({ "direction": "row", "height": "fill" }),
            ),
            (
                parts::CONVERSATION,
                "chat.conversation",
                "conversation",
                json!({ "width": "fill" }),
            ),
            (parts::PROMPT, "chat.prompt", "prompt", json!({})),
            (parts::STATUS, "chat.status", "status", json!({})),
        ];
        for (node, kind, id, extra) in parts {
            let mut props: Props = serde_json::from_value(extra).unwrap_or_default();
            props.insert("id".into(), Value::String(id.into()));
            tree.apply(&Op::Create {
                node,
                kind: kind.into(),
                props,
                style: Props::new(),
            });
        }
        for (parent, node, index) in [
            (parts::CHAT, parts::HEADER, 0),
            (parts::HEADER, parts::HEADER_RIGHT, 0),
            (parts::CHAT, parts::MAIN, 1),
            (parts::MAIN, parts::CONVERSATION, 0),
            (parts::CHAT, parts::PROMPT, 2),
            (parts::CHAT, parts::STATUS, 3),
        ] {
            tree.apply(&Op::Insert {
                parent,
                node,
                index,
            });
        }
        tree.root = Some(parts::CHAT);
        tree
    }

    /// Whether nothing differs from [`Tree::default_chat`], so a client can
    /// draw its usual screen.
    pub fn is_default(&self) -> bool {
        let default = Tree::default_chat();
        self.root == default.root
            && self.bindings.is_empty()
            && self.entries.is_empty()
            && self.reachable() == default.reachable()
    }

    /// Nodes under the root, in drawing order.
    pub fn reachable(&self) -> Vec<(NodeId, &Node)> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.walk(root, &mut out);
        }
        out
    }

    fn walk<'a>(&'a self, node: NodeId, out: &mut Vec<(NodeId, &'a Node)>) {
        if let Some(n) = self.nodes.get(&node) {
            out.push((node, n));
            for child in &n.children {
                self.walk(*child, out);
            }
        }
    }

    pub fn get(&self, node: NodeId) -> Option<&Node> {
        self.nodes.get(&node)
    }

    /// The node whose `id` property is `id`, if it is in the tree.
    pub fn find(&self, id: &str) -> Option<NodeId> {
        let mut found: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.props.get("id").and_then(Value::as_str) == Some(id))
            .map(|(k, _)| *k)
            .collect();
        found.sort();
        found.first().copied()
    }

    /// Whether `node` is `ancestor` or sits somewhere under it.
    pub fn is_within(&self, node: NodeId, ancestor: NodeId) -> bool {
        let mut at = Some(node);
        while let Some(n) = at {
            if n == ancestor {
                return true;
            }
            at = self.nodes.get(&n).and_then(|n| n.parent);
        }
        false
    }

    fn detach(&mut self, node: NodeId) {
        let Some(parent) = self.nodes.get_mut(&node).and_then(|n| n.parent.take()) else {
            return;
        };
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.retain(|c| *c != node);
        }
    }

    /// Apply one op. Ops naming unknown nodes are ignored.
    pub fn apply(&mut self, op: &Op) {
        match op {
            Op::Create {
                node,
                kind,
                props,
                style,
            } => {
                self.nodes.insert(
                    *node,
                    Node {
                        kind: kind.clone(),
                        props: props.clone(),
                        style: style.clone(),
                        children: Vec::new(),
                        parent: None,
                    },
                );
            }
            Op::Set { node, key, value } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    set_or_remove(&mut n.props, key, value);
                }
            }
            Op::Style { node, key, value } => {
                if let Some(n) = self.nodes.get_mut(node) {
                    set_or_remove(&mut n.style, key, value);
                }
            }
            Op::Insert {
                parent,
                node,
                index,
            } => {
                if !self.nodes.contains_key(parent)
                    || !self.nodes.contains_key(node)
                    || self.is_within(*parent, *node)
                {
                    return;
                }
                self.detach(*node);
                if let Some(p) = self.nodes.get_mut(parent) {
                    let at = (*index).min(p.children.len());
                    p.children.insert(at, *node);
                }
                if let Some(n) = self.nodes.get_mut(node) {
                    n.parent = Some(*parent);
                }
            }
            Op::Remove { node } => self.detach(*node),
            Op::Destroy { node } => {
                self.detach(*node);
                let mut stack = vec![*node];
                while let Some(n) = stack.pop() {
                    if let Some(gone) = self.nodes.remove(&n) {
                        stack.extend(gone.children);
                    }
                    self.screens.remove(&n);
                }
            }
            Op::Root { node } => self.root = Some(*node),
            Op::Bindings { keys } => self.bindings = keys.clone(),
            Op::Entry { key, decoration } => {
                if *decoration == EntryDecoration::default() {
                    self.entries.remove(key);
                } else {
                    self.entries.insert(key.clone(), decoration.clone());
                }
            }
            Op::Screen { node, screen } => {
                self.screens.insert(*node, screen.clone());
            }
            Op::Note { .. } => {}
        }
    }

    /// Ops that rebuild this tree from nothing.
    pub fn snapshot(&self) -> Vec<Op> {
        let mut ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        ids.sort();
        let mut ops: Vec<Op> = ids
            .iter()
            .map(|id| {
                let n = &self.nodes[id];
                Op::Create {
                    node: *id,
                    kind: n.kind.clone(),
                    props: n.props.clone(),
                    style: n.style.clone(),
                }
            })
            .collect();
        for id in &ids {
            for (index, child) in self.nodes[id].children.iter().enumerate() {
                ops.push(Op::Insert {
                    parent: *id,
                    node: *child,
                    index,
                });
            }
        }
        if let Some(root) = self.root {
            ops.push(Op::Root { node: root });
        }
        if !self.bindings.is_empty() {
            ops.push(Op::Bindings {
                keys: self.bindings.clone(),
            });
        }
        let mut keys: Vec<&String> = self.entries.keys().collect();
        keys.sort();
        for key in keys {
            ops.push(Op::Entry {
                key: key.clone(),
                decoration: self.entries[key].clone(),
            });
        }
        for (node, screen) in &self.screens {
            ops.push(Op::Screen {
                node: *node,
                screen: screen.clone(),
            });
        }
        ops
    }
}

fn set_or_remove(map: &mut Props, key: &str, value: &Value) {
    if value.is_null() {
        map.remove(key);
    } else {
        map.insert(key.to_string(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_rebuilds_the_same_tree() {
        let mut tree = Tree::default_chat();
        tree.apply(&Op::Create {
            node: 100,
            kind: "text".into(),
            props: Props::new(),
            style: Props::new(),
        });
        tree.apply(&Op::Insert {
            parent: parts::MAIN,
            node: 100,
            index: 9,
        });
        tree.apply(&Op::Style {
            node: 100,
            key: "fg".into(),
            value: json!("accent"),
        });
        assert!(!tree.is_default());
        let mut copy = Tree::default();
        for op in tree.snapshot() {
            copy.apply(&op);
        }
        assert_eq!(copy, tree);
        assert!(Tree::default_chat().is_default());
    }

    #[test]
    fn a_node_cannot_be_inserted_under_itself() {
        let mut tree = Tree::default_chat();
        tree.apply(&Op::Insert {
            parent: parts::CONVERSATION,
            node: parts::MAIN,
            index: 0,
        });
        assert_eq!(tree.get(parts::MAIN).unwrap().parent, Some(parts::CHAT));
    }
}

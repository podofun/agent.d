//! A small flexbox. A `box` (and a `list`) lays its children out in a row
//! or a column; sizes are `fill`, a percentage, or a number of cells, and
//! anything else takes the size its content needs.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tree::Node;
use crate::{NodeId, Tree};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.width && y < self.y + self.height
    }

    /// The area inside a border and padding.
    pub fn inset(&self, by: u16) -> Rect {
        Rect::new(
            self.x.saturating_add(by),
            self.y.saturating_add(by),
            self.width.saturating_sub(2 * by),
            self.height.saturating_sub(2 * by),
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Size {
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Dim {
    Fill,
    Percent(f64),
    Cells(u16),
    Auto,
}

fn dim(value: Option<&Value>) -> Dim {
    match value {
        Some(Value::Number(n)) => Dim::Cells(n.as_f64().unwrap_or(0.0).clamp(0.0, 10_000.0) as u16),
        Some(Value::String(s)) if s == "fill" => Dim::Fill,
        Some(Value::String(s)) if s.ends_with('%') => s[..s.len() - 1]
            .trim()
            .parse::<f64>()
            .map(|p| Dim::Percent(p.clamp(0.0, 100.0)))
            .unwrap_or(Dim::Auto),
        _ => Dim::Auto,
    }
}

fn number(node: &Node, key: &str) -> u16 {
    node.props
        .get(key)
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        .clamp(0.0, 1_000.0) as u16
}

pub(crate) fn visible(node: &Node) -> bool {
    node.props.get("visible") != Some(&Value::Bool(false))
}

/// Cells a node's border takes on each side.
pub fn border_width(node: &Node) -> u16 {
    match node.style.get("border").and_then(Value::as_str) {
        Some("line") | Some("rounded") | Some("double") | Some("thick") => 1,
        _ => 0,
    }
}

fn is_container(node: &Node) -> bool {
    matches!(node.kind.as_str(), "box" | "list")
}

fn is_row(node: &Node) -> bool {
    node.props.get("direction").and_then(Value::as_str) == Some("row")
}

/// Where each visible element under the root goes in `area`, parents
/// before their children. `measure` gives the size an element that is not
/// a container needs, given the width it may use.
pub fn layout(
    tree: &Tree,
    area: Rect,
    measure: &dyn Fn(NodeId, &Node, u16) -> Size,
) -> Vec<(NodeId, Rect)> {
    let mut out = Vec::new();
    if let Some(root) = tree.root
        && let Some(node) = tree.get(root)
        && visible(node)
    {
        place(tree, root, area, measure, &mut out);
    }
    out
}

fn place(
    tree: &Tree,
    id: NodeId,
    area: Rect,
    measure: &dyn Fn(NodeId, &Node, u16) -> Size,
    out: &mut Vec<(NodeId, Rect)>,
) {
    let Some(node) = tree.get(id) else { return };
    out.push((id, area));
    if node.kind == "chat.header" {
        place_header(tree, node, area, measure, out);
        return;
    }
    if !is_container(node) {
        return;
    }
    let inner = area.inset(border_width(node) + number(node, "padding"));
    let row = is_row(node);
    let gap = number(node, "gap");
    let kids: Vec<(NodeId, &Node)> = node
        .children
        .iter()
        .filter_map(|c| tree.get(*c).map(|n| (*c, n)))
        .filter(|(_, n)| visible(n))
        .collect();
    if kids.is_empty() {
        return;
    }
    let main_total = if row { inner.width } else { inner.height };
    let cross_total = if row { inner.height } else { inner.width };
    let gaps = gap.saturating_mul(kids.len().saturating_sub(1) as u16);
    let available = main_total.saturating_sub(gaps);

    // Fixed sizes first, then what is left goes to `fill` children equally.
    let mut sizes: Vec<Option<u16>> = Vec::with_capacity(kids.len());
    let mut fills = 0u16;
    let mut used = 0u16;
    for (kid, n) in &kids {
        let main = if row {
            n.props.get("width")
        } else {
            n.props.get("height")
        };
        let size = match dim(main).or_default_for(n) {
            Dim::Fill => {
                fills += 1;
                None
            }
            Dim::Percent(p) => Some((f64::from(available) * p / 100.0).round() as u16),
            Dim::Cells(c) => Some(c),
            Dim::Auto => {
                let natural = natural(tree, *kid, n, inner.width, measure);
                Some(if row { natural.width } else { natural.height })
            }
        };
        if let Some(s) = size {
            used = used.saturating_add(s);
        }
        sizes.push(size);
    }
    let left = available.saturating_sub(used);
    let share = left.checked_div(fills).unwrap_or(0);
    let mut extra = left.checked_rem(fills).unwrap_or(0);

    let mut at = if row { inner.x } else { inner.y };
    let end = if row {
        inner.x + inner.width
    } else {
        inner.y + inner.height
    };
    for ((kid, n), size) in kids.iter().zip(sizes) {
        let mut main = size.unwrap_or_else(|| {
            let bonus = u16::from(extra > 0);
            extra = extra.saturating_sub(1);
            share + bonus
        });
        main = main.min(end.saturating_sub(at));
        let cross_dim = if row {
            n.props.get("height")
        } else {
            n.props.get("width")
        };
        let cross = match dim(cross_dim) {
            Dim::Cells(c) => c.min(cross_total),
            Dim::Percent(p) => {
                ((f64::from(cross_total) * p / 100.0).round() as u16).min(cross_total)
            }
            _ => cross_total,
        };
        let rect = if row {
            Rect::new(at, inner.y, main, cross)
        } else {
            Rect::new(inner.x, at, cross, main)
        };
        place(tree, *kid, rect, measure, out);
        at = at.saturating_add(main).saturating_add(gap);
    }
}

/// The header's children sit at the right end of its first line, each as
/// wide as it needs, the last one two cells from the edge as the chat's name
/// is on the left.
fn place_header(
    tree: &Tree,
    node: &Node,
    area: Rect,
    measure: &dyn Fn(NodeId, &Node, u16) -> Size,
    out: &mut Vec<(NodeId, Rect)>,
) {
    let mut right = area.x.saturating_add(area.width.saturating_sub(2));
    let mut placed = Vec::new();
    for child in node.children.iter().rev() {
        let Some(n) = tree.get(*child).filter(|n| visible(n)) else {
            continue;
        };
        let width = natural(tree, *child, n, area.width, measure)
            .width
            .min(right.saturating_sub(area.x));
        right -= width;
        placed.push((*child, Rect::new(right, area.y, width, area.height.min(1))));
    }
    for (child, rect) in placed.into_iter().rev() {
        place(tree, child, rect, measure, out);
    }
}

trait DefaultFor {
    fn or_default_for(self, node: &Node) -> Dim;
}

impl DefaultFor for Dim {
    /// Spacers and terminals grow to fill when no size is given.
    fn or_default_for(self, node: &Node) -> Dim {
        match (self, node.kind.as_str()) {
            (Dim::Auto, "spacer" | "terminal") => Dim::Fill,
            (d, _) => d,
        }
    }
}

/// The size a node needs: a container adds up its children.
fn natural(
    tree: &Tree,
    id: NodeId,
    node: &Node,
    width: u16,
    measure: &dyn Fn(NodeId, &Node, u16) -> Size,
) -> Size {
    let fixed = |key: &str| match dim(node.props.get(key)) {
        Dim::Cells(c) => Some(c),
        _ => None,
    };
    if !is_container(node) {
        let m = measure(id, node, width);
        return Size {
            width: fixed("width").unwrap_or(m.width),
            height: fixed("height").unwrap_or(m.height),
        };
    }
    let edge = 2 * (border_width(node) + number(node, "padding"));
    let gap = number(node, "gap");
    let row = is_row(node);
    let mut main = 0u16;
    let mut cross = 0u16;
    let mut count = 0u16;
    for child in &node.children {
        let Some(n) = tree.get(*child).filter(|n| visible(n)) else {
            continue;
        };
        let s = natural(tree, *child, n, width.saturating_sub(edge), measure);
        let (m, c) = if row {
            (s.width, s.height)
        } else {
            (s.height, s.width)
        };
        main = main.saturating_add(m);
        cross = cross.max(c);
        count += 1;
    }
    main = main.saturating_add(gap.saturating_mul(count.saturating_sub(1)));
    let (w, h) = if row { (main, cross) } else { (cross, main) };
    Size {
        width: fixed("width").unwrap_or(w.saturating_add(edge)),
        height: fixed("height").unwrap_or(h.saturating_add(edge)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Op, Props, parts};
    use serde_json::json;

    fn measure(_: NodeId, node: &Node, _: u16) -> Size {
        match node.kind.as_str() {
            "chat.header" | "chat.status" => Size {
                width: 0,
                height: 2,
            },
            "chat.prompt" => Size {
                width: 0,
                height: 3,
            },
            "text" => Size {
                width: 5,
                height: 1,
            },
            _ => Size::default(),
        }
    }

    fn rect_of(rects: &[(NodeId, Rect)], id: NodeId) -> Rect {
        rects.iter().find(|(n, _)| *n == id).unwrap().1
    }

    #[test]
    fn the_default_chat_view_stacks_its_parts() {
        let tree = Tree::default_chat();
        let rects = layout(&tree, Rect::new(0, 0, 80, 24), &measure);
        assert_eq!(rect_of(&rects, parts::HEADER), Rect::new(0, 0, 80, 2));
        assert_eq!(
            rect_of(&rects, parts::CONVERSATION),
            Rect::new(0, 2, 80, 17)
        );
        assert_eq!(rect_of(&rects, parts::PROMPT), Rect::new(0, 19, 80, 3));
        assert_eq!(rect_of(&rects, parts::STATUS), Rect::new(0, 22, 80, 2));
        assert_eq!(rect_of(&rects, parts::HEADER_RIGHT), Rect::new(78, 0, 0, 1));
    }

    #[test]
    fn what_a_script_adds_to_the_header_sits_at_its_right() {
        let mut tree = Tree::default_chat();
        for node in [100, 101] {
            tree.apply(&Op::Create {
                node,
                kind: "text".into(),
                props: Props::new(),
                style: Props::new(),
            });
            tree.apply(&Op::Insert {
                parent: parts::HEADER_RIGHT,
                node,
                index: 9,
            });
        }
        let rects = layout(&tree, Rect::new(0, 0, 80, 24), &measure);
        assert_eq!(
            rect_of(&rects, parts::HEADER_RIGHT),
            Rect::new(67, 0, 11, 1)
        );
        assert_eq!(rect_of(&rects, 100), Rect::new(67, 0, 5, 1));
        assert_eq!(rect_of(&rects, 101), Rect::new(73, 0, 5, 1));
        assert_eq!(rect_of(&rects, parts::CONVERSATION).y, 2);
    }

    #[test]
    fn a_panel_beside_the_conversation_takes_its_width_and_the_rest_fills() {
        let mut tree = Tree::default_chat();
        let mut props = Props::new();
        props.insert("width".into(), json!(30));
        props.insert("padding".into(), json!(1));
        let mut style = Props::new();
        style.insert("border".into(), json!("rounded"));
        tree.apply(&Op::Create {
            node: 100,
            kind: "box".into(),
            props,
            style,
        });
        tree.apply(&Op::Insert {
            parent: parts::MAIN,
            node: 100,
            index: 1,
        });
        tree.apply(&Op::Create {
            node: 101,
            kind: "text".into(),
            props: Props::new(),
            style: Props::new(),
        });
        tree.apply(&Op::Insert {
            parent: 100,
            node: 101,
            index: 0,
        });
        let mut half = Props::new();
        half.insert("width".into(), json!("25%"));
        tree.apply(&Op::Create {
            node: 102,
            kind: "spacer".into(),
            props: half,
            style: Props::new(),
        });
        tree.apply(&Op::Insert {
            parent: parts::MAIN,
            node: 102,
            index: 0,
        });
        let rects = layout(&tree, Rect::new(0, 0, 100, 24), &measure);
        assert_eq!(rect_of(&rects, 102), Rect::new(0, 2, 25, 17));
        assert_eq!(
            rect_of(&rects, parts::CONVERSATION),
            Rect::new(25, 2, 45, 17)
        );
        assert_eq!(rect_of(&rects, 100), Rect::new(70, 2, 30, 17));
        // Inside the border and the padding.
        assert_eq!(rect_of(&rects, 101), Rect::new(72, 4, 26, 1));

        tree.apply(&Op::Set {
            node: 100,
            key: "visible".into(),
            value: json!(false),
        });
        let rects = layout(&tree, Rect::new(0, 0, 100, 24), &measure);
        assert!(rects.iter().all(|(n, _)| *n != 100 && *n != 101));
        assert_eq!(rect_of(&rects, parts::CONVERSATION).width, 75);
    }
}

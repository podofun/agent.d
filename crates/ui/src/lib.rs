//! Scripted chat UI. Lua in the daemon builds a tree of elements per chat;
//! clients draw it and send back what the user does. This crate owns the
//! element tree, the typed operations and frames that carry it over `/ws`,
//! the layout every client shares, and the per-chat frame loop.

mod doc;
mod hub;
mod layout;
mod terminal;
mod tree;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use doc::Document;
pub use hub::{Chat, ChatInfo, Hub, Script, Subscription};
pub use layout::{Rect, Size, border_width, layout};
pub use terminal::{TerminalIo, TerminalResize};
pub use tree::{Node, Tree};

/// Version of the wire protocol. A client sends the version it speaks in
/// `ui.subscribe`; the daemon answers with its own in `ui.ready`.
pub const PROTOCOL_VERSION: u32 = 1;

pub type NodeId = u64;
pub type ChatId = u64;
pub type Props = BTreeMap<String, Value>;

/// The core elements every client draws.
pub const CORE_ELEMENTS: [&str; 8] = [
    "box", "text", "button", "input", "list", "checkbox", "terminal", "spacer",
];

/// Ids of chat's own parts in the default `chat` view.
pub mod parts {
    use super::NodeId;
    pub const CHAT: NodeId = 1;
    pub const HEADER: NodeId = 2;
    pub const MAIN: NodeId = 3;
    pub const CONVERSATION: NodeId = 4;
    pub const PROMPT: NodeId = 5;
    pub const STATUS: NodeId = 6;
    /// Script elements are numbered from here.
    pub const FIRST_FREE: NodeId = 100;
}

/// Keys that always belong to the client: interrupt, approval mode
/// switching and prompt submit. Approval window keys are reserved by the
/// client while the window is open.
pub const RESERVED_KEYS: [&str; 4] = ["ctrl+c", "shift+tab", "backtab", "enter"];

/// Whether `key` (already normalized) is one scripts may not bind.
pub fn is_reserved_key(key: &str) -> bool {
    RESERVED_KEYS.contains(&key)
}

/// Normalize a key name such as `Ctrl+G` to `ctrl+g`, with modifiers in
/// the order ctrl, alt, shift.
pub fn normalize_key(key: &str) -> String {
    let lower = key.trim().to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').filter(|p| !p.is_empty()).collect();
    if lower.ends_with("++") {
        parts.push("+");
    }
    let Some(last) = parts.pop() else {
        return lower;
    };
    let mut out = String::new();
    for modifier in ["ctrl", "alt", "shift"] {
        if parts
            .iter()
            .any(|p| *p == modifier || (modifier == "ctrl" && *p == "control"))
        {
            out.push_str(modifier);
            out.push('+');
        }
    }
    out.push_str(match last {
        "esc" => "escape",
        "return" => "enter",
        other => other,
    });
    out
}

/// Replace control characters so script text cannot carry terminal escape
/// sequences. Newlines are kept; tabs become spaces.
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => '\n',
            '\t' => ' ',
            c if c.is_control() => '\u{fffd}',
            c => c,
        })
        .collect()
}

/// One change to a chat's UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// A new, detached element.
    Create {
        node: NodeId,
        kind: String,
        #[serde(default, skip_serializing_if = "Props::is_empty")]
        props: Props,
        #[serde(default, skip_serializing_if = "Props::is_empty")]
        style: Props,
    },
    /// Set a property; `null` removes it.
    Set {
        node: NodeId,
        key: String,
        value: Value,
    },
    /// Set a style; `null` removes it.
    Style {
        node: NodeId,
        key: String,
        value: Value,
    },
    /// Put `node` under `parent` at `index`, moving it if it was elsewhere.
    Insert {
        parent: NodeId,
        node: NodeId,
        index: usize,
    },
    /// Detach `node` from its parent. It can be inserted again.
    Remove { node: NodeId },
    /// Forget `node` and everything under it.
    Destroy { node: NodeId },
    /// Show the tree under `node`, the root of the current view.
    Root { node: NodeId },
    /// The shortcuts scripts bound, as normalized key names.
    Bindings { keys: Vec<String> },
    /// How a conversation entry is shown.
    Entry {
        key: String,
        decoration: EntryDecoration,
    },
    /// The screen of a `terminal` element.
    Screen { node: NodeId, screen: Screen },
    /// A message for the user, such as a script error.
    Note { text: String },
}

/// All changes that land together, shown at `at` (daemon milliseconds).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub seq: u64,
    pub at: u64,
    /// The ops rebuild the whole UI: drop everything first.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset: bool,
    /// Drop any frames already received for later than `at`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel_ahead: bool,
    pub ops: Vec<Op>,
}

/// What a client says about itself when it subscribes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub kind: String,
    #[serde(default)]
    pub width: u16,
    #[serde(default)]
    pub height: u16,
    /// Extra elements the client draws, such as `web.editor`.
    #[serde(default)]
    pub supports: Vec<String>,
}

/// `ui.subscribe` params.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeParams {
    #[serde(default)]
    pub version: Option<u32>,
    pub client: ClientInfo,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

/// The first event on a subscription.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ready {
    pub version: u32,
    pub chat: ChatId,
    /// The daemon's clock, in the unit frames are stamped with.
    pub clock: u64,
}

/// Something the user did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Click,
    Key {
        key: String,
    },
    Change {
        value: Value,
    },
    Focus,
    Blur,
    Hover,
    /// Typed text or key bytes for a `terminal` element.
    Input {
        data: String,
    },
    /// The client, or a `terminal` element, changed size.
    Resize {
        width: u16,
        height: u16,
    },
}

impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Click => "click",
            Event::Key { .. } => "key",
            Event::Change { .. } => "change",
            Event::Focus => "focus",
            Event::Blur => "blur",
            Event::Hover => "hover",
            Event::Input { .. } => "input",
            Event::Resize { .. } => "resize",
        }
    }
}

/// `ui.event` params.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventParams {
    pub chat: ChatId,
    #[serde(default)]
    pub node: Option<NodeId>,
    #[serde(flatten)]
    pub event: Event,
}

/// `ui.session` params: the chat opened another session, or a new chat.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionParams {
    pub chat: ChatId,
    #[serde(default)]
    pub session: Option<String>,
}

/// `ui.entry` params: a conversation entry the client shows.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryParams {
    pub chat: ChatId,
    pub entry: EntryInfo,
}

/// What scripts may read about a conversation entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryInfo {
    /// Stable within the chat until it switches sessions.
    pub key: String,
    /// `user`, `agent`, `tool`, `info`, `error` or `setting`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// On a tool row: the action or tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Value>,
}

/// How a script changed the way an entry is shown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryDecoration {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Props::is_empty")]
    pub style: Props,
    /// Elements drawn above the entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub before: Vec<NodeId>,
    /// Elements drawn below the entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<NodeId>,
}

/// A terminal's screen as a grid of styled runs, one list per row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub width: u16,
    pub height: u16,
    pub lines: Vec<Vec<Run>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<(u16, u16)>,
}

/// Characters that share one style.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub underline: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inverse: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_normalize_and_reserved_ones_are_known() {
        assert_eq!(normalize_key("Ctrl+G"), "ctrl+g");
        assert_eq!(normalize_key("shift+ctrl+x"), "ctrl+shift+x");
        assert_eq!(normalize_key("Control+C"), "ctrl+c");
        assert!(is_reserved_key(&normalize_key("CTRL+C")));
        assert!(is_reserved_key(&normalize_key("Return")));
        assert!(!is_reserved_key("ctrl+g"));
    }

    #[test]
    fn script_text_cannot_carry_terminal_escapes() {
        let clean = sanitize("hi\x1b[2J\x07there\u{9b}31m\nnext\tcol");
        assert!(!clean.chars().any(|c| c.is_control() && c != '\n'));
        assert!(clean.contains("hi") && clean.contains("there") && clean.contains("\nnext col"));
    }
}

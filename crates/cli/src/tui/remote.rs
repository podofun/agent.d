//! Scripted UI: one `/ws` connection that subscribes to this chat's UI,
//! receives frames, and sends back what the user does. Without a daemon,
//! or when it sends nothing, the chat keeps its usual screen.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use agentd_ui::{ChatId, Event, Frame, NodeId, Op, Ready, Tree};
use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::app::Update;
use crate::ws::{resolve_ws_token, ws_url_of};

/// What the UI connection reports to the chat.
pub(super) enum Remote {
    Ready(Ready),
    Frame(Frame),
}

/// Requests for the daemon, as method and params.
type Outgoing = mpsc::UnboundedSender<(&'static str, Value)>;

pub(super) fn start(
    base: String,
    timeout: u64,
    hello: Value,
    updates: mpsc::UnboundedSender<Update>,
) -> Outgoing {
    let (out, requests) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        // A daemon without scripted UI, or none at all, leaves chat as it is.
        let _ = listen(&base, timeout, hello, &updates, requests).await;
    });
    out
}

async fn listen(
    base: &str,
    timeout: u64,
    hello: Value,
    updates: &mpsc::UnboundedSender<Update>,
    mut requests: mpsc::UnboundedReceiver<(&'static str, Value)>,
) -> Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = ws_url_of(base)?;
    let mut request = url.as_str().into_client_request()?;
    if let Some(token) = resolve_ws_token() {
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse()?);
    }
    let (mut socket, _) = tokio::time::timeout(
        Duration::from_millis(timeout),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .context("the UI connection timed out")??;
    let subscribe = json!({ "id": 1, "method": "ui.subscribe", "params": hello });
    socket
        .send(Message::Text(subscribe.to_string().into()))
        .await?;
    let mut next_id = 2u64;
    loop {
        tokio::select! {
            message = socket.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    Some(Ok(Message::Binary(bytes))) => String::from_utf8_lossy(&bytes).into_owned(),
                    Some(Ok(Message::Close(_))) | None => return Err(anyhow!("the UI connection closed")),
                    Some(Ok(_)) => continue,
                    Some(Err(error)) => return Err(error.into()),
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                let remote = match value["event"].as_str() {
                    Some("ui.ready") => serde_json::from_value(value["ready"].clone()).ok().map(Remote::Ready),
                    Some("ui.frame") => serde_json::from_value(value["frame"].clone()).ok().map(Remote::Frame),
                    _ => None,
                };
                if let Some(remote) = remote {
                    let _ = updates.send(Update::Ui(remote));
                } else if value["id"] == 1 {
                    // The daemon has no scripted UI, or ended it.
                    return Ok(());
                }
            }
            request = requests.recv() => {
                let Some((method, params)) = request else { return Ok(()) };
                let body = json!({ "id": next_id, "method": method, "params": params });
                next_id += 1;
                socket.send(Message::Text(body.to_string().into())).await?;
            }
        }
    }
}

/// The chat's copy of its scripted UI.
pub(super) struct ScriptUi {
    pub(super) tree: Tree,
    pub(super) chat: Option<ChatId>,
    /// Frames received for later, in the order they are shown.
    queue: Vec<Frame>,
    /// The daemon's clock at a moment of ours.
    clock: Option<(u64, Instant)>,
    pub(super) focus: Option<NodeId>,
    pub(super) hover: Option<NodeId>,
    /// How far down each scrolled container is, by the user's wheel.
    pub(super) scroll: HashMap<NodeId, u16>,
    out: Option<Outgoing>,
    /// Sizes of terminal elements as last sent.
    sizes: HashMap<NodeId, (u16, u16)>,
    pub(super) sent_session: Option<Option<String>>,
    pub(super) sent_entries: Vec<u64>,
}

impl Default for ScriptUi {
    fn default() -> Self {
        Self {
            tree: Tree::default_chat(),
            chat: None,
            queue: Vec::new(),
            clock: None,
            focus: None,
            hover: None,
            scroll: HashMap::new(),
            out: None,
            sizes: HashMap::new(),
            sent_session: None,
            sent_entries: Vec::new(),
        }
    }
}

impl ScriptUi {
    pub(super) fn connect(&mut self, out: Outgoing) {
        self.out = Some(out);
    }

    /// Whether the screen differs from chat's own layout.
    pub(super) fn custom(&self) -> bool {
        let default = Tree::default_chat();
        self.chat.is_some()
            && (self.tree.root != default.root || self.tree.reachable() != default.reachable())
    }

    pub(super) fn send(&self, method: &'static str, mut params: Value) {
        if let (Some(out), Some(chat)) = (&self.out, self.chat) {
            params["chat"] = json!(chat);
            let _ = out.send((method, params));
        }
    }

    pub(super) fn event(&self, node: Option<NodeId>, event: Event) {
        let mut params = serde_json::to_value(&event).unwrap_or_default();
        if let Some(node) = node {
            params["node"] = json!(node);
        }
        self.send("ui.event", params);
    }

    /// Whether frames wait to be shown.
    pub(super) fn waiting(&self) -> bool {
        !self.queue.is_empty()
    }

    pub(super) fn ready(&mut self, ready: Ready) {
        self.chat = Some(ready.chat);
        self.clock = Some((ready.clock, Instant::now()));
        self.sent_session = None;
        self.sent_entries.clear();
    }

    pub(super) fn receive(&mut self, frame: Frame) {
        if frame.cancel_ahead {
            self.queue.retain(|f| f.at <= frame.at);
        }
        if frame.reset {
            self.queue.clear();
        }
        let at = self
            .queue
            .partition_point(|f| (f.at, f.seq) <= (frame.at, frame.seq));
        self.queue.insert(at, frame);
    }

    fn daemon_now(&self) -> u64 {
        self.clock.map_or(u64::MAX, |(at, then)| {
            at + then.elapsed().as_millis() as u64
        })
    }

    /// Apply every frame that is due, skipping straight to the newest
    /// state when several are. Returns the ops the chat must act on: notes,
    /// entry decorations, and `None` for a reset.
    pub(super) fn play(&mut self) -> Option<Vec<Option<Op>>> {
        let now = self.daemon_now();
        let due = self.queue.partition_point(|f| f.at <= now);
        if due == 0 {
            return None;
        }
        let mut out = Vec::new();
        for frame in self.queue.drain(..due).collect::<Vec<_>>() {
            if frame.reset {
                self.tree = Tree::default();
                self.focus = None;
                self.hover = None;
                self.scroll.clear();
                self.sizes.clear();
                out.push(None);
            }
            for op in frame.ops {
                self.tree.apply(&op);
                if matches!(op, Op::Note { .. } | Op::Entry { .. }) {
                    out.push(Some(op));
                }
            }
        }
        if self.focus.is_some_and(|f| self.tree.get(f).is_none()) {
            self.focus = None;
        }
        Some(out)
    }

    /// Tell the daemon the size of each terminal element that changed.
    pub(super) fn terminal_sizes(&mut self, rects: Vec<(NodeId, u16, u16)>) {
        for (node, width, height) in rects {
            if self.sizes.get(&node) != Some(&(width, height)) && width > 0 && height > 0 {
                self.sizes.insert(node, (width, height));
                self.event(Some(node), Event::Resize { width, height });
            }
        }
    }
}

/// The bytes a terminal program expects for a key.
pub(super) fn key_bytes(key: &crossterm::event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let base = match key.code {
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            ((c.to_ascii_lowercase() as u8 - b'a' + 1) as char).to_string()
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Backspace => "\x7f".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::BackTab => "\x1b[Z".into(),
        KeyCode::Esc => "\x1b".into(),
        KeyCode::Up => "\x1b[A".into(),
        KeyCode::Down => "\x1b[B".into(),
        KeyCode::Right => "\x1b[C".into(),
        KeyCode::Left => "\x1b[D".into(),
        KeyCode::Home => "\x1b[H".into(),
        KeyCode::End => "\x1b[F".into(),
        KeyCode::PageUp => "\x1b[5~".into(),
        KeyCode::PageDown => "\x1b[6~".into(),
        KeyCode::Delete => "\x1b[3~".into(),
        KeyCode::Insert => "\x1b[2~".into(),
        KeyCode::F(n @ 1..=4) => format!("\x1bO{}", (b'P' + n - 1) as char),
        KeyCode::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return None,
            };
            format!("\x1b[{code}~")
        }
        _ => return None,
    };
    Some(if alt { format!("\x1b{base}") } else { base })
}

/// A key's name as scripts bind it, such as `ctrl+g`.
pub(super) fn key_name(key: &crossterm::event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let mut name = String::new();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        name.push_str("ctrl+");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        name.push_str("alt+");
    }
    let code = match key.code {
        KeyCode::Char(c) => {
            if c.is_uppercase() {
                name.push_str("shift+");
            }
            c.to_lowercase().to_string()
        }
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::BackTab => return Some("backtab".into()),
        other => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                name.push_str("shift+");
            }
            match other {
                KeyCode::Enter => "enter",
                KeyCode::Esc => "escape",
                KeyCode::Tab => "tab",
                KeyCode::Backspace => "backspace",
                KeyCode::Delete => "delete",
                KeyCode::Up => "up",
                KeyCode::Down => "down",
                KeyCode::Left => "left",
                KeyCode::Right => "right",
                KeyCode::Home => "home",
                KeyCode::End => "end",
                KeyCode::PageUp => "pageup",
                KeyCode::PageDown => "pagedown",
                _ => return None,
            }
            .to_string()
        }
    };
    name.push_str(&code);
    Some(name)
}

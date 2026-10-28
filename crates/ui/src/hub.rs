//! Connected chats and their frame loops. The hub outlives hot reloads: a
//! new runtime installs its [`Script`] and every open chat is rebuilt by it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::terminal::{Terminal, TerminalIo};
use crate::{
    ChatId, ClientInfo, Document, EntryInfo, Event, Frame, NodeId, Op, PROTOCOL_VERSION, Ready,
};

/// How often a chat's changes are gathered into a frame.
const FRAME: Duration = Duration::from_millis(16);
/// How far ahead time-driven frames are rendered.
const LEAD_MS: u64 = 300;
/// Frames waiting for a slow client before it is skipped to the newest state.
const QUEUE: usize = 64;

/// The scripts' side of the UI, installed by the runtime. Calls that run
/// Lua return at once and do their work in the background.
pub trait Script: Send + Sync {
    /// A chat connected, or the scripts were reloaded while it was open.
    fn opened(&self, chat: Arc<Chat>);
    fn closed(&self, chat: ChatId);
    fn event(&self, chat: Arc<Chat>, node: Option<NodeId>, event: Event);
    /// The chat switched sessions.
    fn session(&self, chat: Arc<Chat>);
    fn entry(&self, chat: Arc<Chat>, entry: EntryInfo);
    /// Frames per second this chat animates at, if it does.
    fn animates(&self, chat: ChatId) -> Option<u32>;
    /// Run the chat's frame functions for `t_ms` after it connected. Called
    /// on a blocking thread; returns the changes they made.
    fn render(&self, chat: &Arc<Chat>, t_ms: u64) -> Vec<Op>;
}

/// Who a chat is and where it works.
#[derive(Debug, Clone, Default)]
pub struct ChatInfo {
    pub interface: String,
    pub user: Option<String>,
    /// The `/ws` connection that subscribed.
    pub connection: String,
    pub session: Option<String>,
    pub folder: Option<String>,
    pub client: ClientInfo,
}

pub struct Chat {
    pub id: ChatId,
    /// Daemon time the chat connected, the zero of its frame functions.
    pub started: u64,
    info: RwLock<ChatInfo>,
    doc: Mutex<Document>,
    immediate: Mutex<Vec<Op>>,
    cancel: AtomicBool,
    terminals: Mutex<HashMap<NodeId, Terminal>>,
}

impl Chat {
    pub fn info(&self) -> ChatInfo {
        self.info.read().unwrap().clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut ChatInfo)) {
        f(&mut self.info.write().unwrap());
    }

    pub fn doc(&self) -> MutexGuard<'_, Document> {
        self.doc.lock().unwrap()
    }

    /// Show `text` to the user, as for a script error.
    pub fn note(&self, text: &str) {
        self.doc().note(text);
    }

    /// Run `io` in the `terminal` element `node`, replacing what ran there.
    pub fn open_terminal(self: &Arc<Self>, node: NodeId, io: TerminalIo, width: u16, height: u16) {
        let chat = Arc::downgrade(self);
        let terminal = Terminal::start(io, width, height, move |screen| {
            if let Some(chat) = chat.upgrade() {
                chat.immediate
                    .lock()
                    .unwrap()
                    .push(Op::Screen { node, screen });
            }
        });
        self.terminals.lock().unwrap().insert(node, terminal);
    }

    pub fn close_terminal(&self, node: NodeId) {
        self.terminals.lock().unwrap().remove(&node);
    }

    pub fn close_terminals(&self) {
        self.terminals.lock().unwrap().clear();
    }

    pub fn has_terminal(&self, node: NodeId) -> bool {
        self.terminals.lock().unwrap().contains_key(&node)
    }
}

pub struct Hub {
    epoch: Instant,
    next: AtomicU64,
    chats: RwLock<BTreeMap<ChatId, Arc<Chat>>>,
    script: RwLock<Option<Arc<dyn Script>>>,
}

/// An open chat. Frames arrive on `frames`; dropping it closes the chat.
pub struct Subscription {
    pub chat: Arc<Chat>,
    pub ready: Ready,
    pub frames: mpsc::Receiver<Frame>,
    hub: Arc<Hub>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
        self.hub.close(self.chat.id);
    }
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            epoch: Instant::now(),
            next: AtomicU64::new(1),
            chats: RwLock::new(BTreeMap::new()),
            script: RwLock::new(None),
        })
    }

    /// Milliseconds on the daemon's frame clock.
    pub fn now(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    fn script(&self) -> Option<Arc<dyn Script>> {
        self.script.read().unwrap().clone()
    }

    /// Install the scripts of a freshly loaded runtime. Every open chat goes
    /// back to the default view and is built again by them.
    pub fn set_script(&self, script: Option<Arc<dyn Script>>) {
        *self.script.write().unwrap() = script.clone();
        for chat in self.chats() {
            chat.close_terminals();
            chat.doc().reset();
            if let Some(script) = &script {
                script.opened(chat);
            }
        }
    }

    pub fn subscribe(self: &Arc<Self>, info: ChatInfo) -> Subscription {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let chat = Arc::new(Chat {
            id,
            started: self.now(),
            info: RwLock::new(info),
            doc: Mutex::new(Document::new()),
            immediate: Mutex::new(Vec::new()),
            cancel: AtomicBool::new(false),
            terminals: Mutex::new(HashMap::new()),
        });
        self.chats.write().unwrap().insert(id, chat.clone());
        let (tx, frames) = mpsc::channel(QUEUE);
        let task = tokio::spawn(frame_loop(self.clone(), chat.clone(), tx));
        if let Some(script) = self.script() {
            script.opened(chat.clone());
        }
        Subscription {
            ready: Ready {
                version: PROTOCOL_VERSION,
                chat: id,
                clock: self.now(),
            },
            chat,
            frames,
            hub: self.clone(),
            task,
        }
    }

    fn close(&self, id: ChatId) {
        if let Some(chat) = self.chats.write().unwrap().remove(&id) {
            chat.close_terminals();
        }
        if let Some(script) = self.script() {
            script.closed(id);
        }
    }

    pub fn chat(&self, id: ChatId) -> Option<Arc<Chat>> {
        self.chats.read().unwrap().get(&id).cloned()
    }

    pub fn chats(&self) -> Vec<Arc<Chat>> {
        self.chats.read().unwrap().values().cloned().collect()
    }

    /// Something the user did. Terminal input and sizes go straight to the
    /// program; everything else reaches the scripts and replaces frames
    /// already sent ahead.
    pub fn event(&self, chat: &Arc<Chat>, node: Option<NodeId>, event: Event) {
        if let Some(node) = node
            && let Some(terminal) = chat.terminals.lock().unwrap().get(&node)
        {
            match &event {
                Event::Input { data } => return terminal.input(data),
                Event::Resize { width, height } => return terminal.resize(*width, *height),
                _ => {}
            }
        }
        if let (None, Event::Resize { width, height }) = (node, &event) {
            chat.update(|info| {
                info.client.width = *width;
                info.client.height = *height;
            });
        }
        chat.cancel.store(true, Ordering::SeqCst);
        if let Some(script) = self.script() {
            script.event(chat.clone(), node, event);
        }
    }

    /// The chat opened `session` (or a new chat), working in `folder`.
    pub fn session(&self, chat: &Arc<Chat>, session: Option<String>, folder: Option<String>) {
        chat.update(|info| {
            info.session = session;
            if folder.is_some() {
                info.folder = folder;
            }
        });
        chat.doc().clear_entries();
        if let Some(script) = self.script() {
            script.session(chat.clone());
        }
    }

    pub fn entry(&self, chat: &Arc<Chat>, entry: EntryInfo) {
        if let Some(script) = self.script() {
            script.entry(chat.clone(), entry);
        }
    }
}

/// Gather the chat's changes every frame and send them as one message, then
/// render time-driven frames ahead so the client can play them on its own.
async fn frame_loop(hub: Arc<Hub>, chat: Arc<Chat>, tx: mpsc::Sender<Frame>) {
    let mut tick = tokio::time::interval(FRAME);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut seq = 0u64;
    let mut backlog: Option<Frame> = None;
    let mut ahead_until = 0u64;
    let mut touched: BTreeSet<(NodeId, bool, String)> = BTreeSet::new();
    loop {
        tick.tick().await;
        let now = hub.now();
        let cancel = chat.cancel.swap(false, Ordering::SeqCst);
        let (reset, mut ops) = {
            let mut doc = chat.doc();
            let (reset, mut ops) = doc.take_pending();
            if cancel && !reset {
                ops.extend(doc.current(&touched));
            }
            (reset, ops)
        };
        if cancel || reset {
            touched.clear();
            // Draw the animation again from this very moment, so it carries
            // on where it was instead of jumping.
            ahead_until = now.saturating_sub(1000);
        }
        ops.append(&mut chat.immediate.lock().unwrap());
        if reset || cancel || !ops.is_empty() || backlog.is_some() {
            seq += 1;
            let frame = match backlog.take() {
                // A client that fell behind gets everything since in one frame.
                Some(mut late) if !reset => {
                    late.ops.extend(ops);
                    late.cancel_ahead |= cancel;
                    late.at = now;
                    late.seq = seq;
                    late
                }
                _ => Frame {
                    seq,
                    at: now,
                    reset,
                    cancel_ahead: cancel,
                    ops,
                },
            };
            match tx.try_send(frame) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(frame)) => backlog = Some(frame),
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
        if backlog.is_some() {
            continue;
        }
        let Some(script) = hub.script() else { continue };
        let Some(fps) = script.animates(chat.id) else {
            continue;
        };
        let step = 1000 / u64::from(fps.clamp(1, 60));
        if ahead_until + step < now {
            ahead_until = now - step;
        }
        while ahead_until + step <= now + LEAD_MS {
            ahead_until += step;
            let t = ahead_until.saturating_sub(chat.started);
            let ops = {
                let (script, chat) = (script.clone(), chat.clone());
                tokio::task::spawn_blocking(move || script.render(&chat, t))
                    .await
                    .unwrap_or_default()
            };
            if ops.is_empty() {
                continue;
            }
            for op in &ops {
                match op {
                    Op::Set { node, key, .. } => touched.insert((*node, false, key.clone())),
                    Op::Style { node, key, .. } => touched.insert((*node, true, key.clone())),
                    _ => false,
                };
            }
            seq += 1;
            let frame = Frame {
                seq,
                at: ahead_until,
                reset: false,
                cancel_ahead: false,
                ops,
            };
            if tx.try_send(frame).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Props, parts};
    use serde_json::json;

    /// Animates a text's colour with time and counts events.
    struct Blink {
        node: NodeId,
        events: Mutex<Vec<String>>,
    }

    impl Script for Blink {
        fn opened(&self, chat: Arc<Chat>) {
            let mut doc = chat.doc();
            let node = doc.create("text", Props::new(), Props::new());
            assert_eq!(node, self.node);
            doc.insert(parts::MAIN, node, 1).unwrap();
        }
        fn closed(&self, _: ChatId) {}
        fn event(&self, chat: Arc<Chat>, _: Option<NodeId>, event: Event) {
            self.events.lock().unwrap().push(event.name().into());
            chat.doc().set(self.node, "text", json!("clicked"));
        }
        fn session(&self, _: Arc<Chat>) {}
        fn entry(&self, _: Arc<Chat>, _: EntryInfo) {}
        fn animates(&self, _: ChatId) -> Option<u32> {
            Some(20)
        }
        fn render(&self, chat: &Arc<Chat>, t: u64) -> Vec<Op> {
            let mut doc = chat.doc();
            doc.capture_begin();
            doc.set_style(self.node, "fg", json!(format!("t{t}")));
            doc.capture_end()
        }
    }

    #[tokio::test]
    async fn frames_render_ahead_and_an_event_cancels_them() {
        let hub = Hub::new();
        let script = Arc::new(Blink {
            node: parts::FIRST_FREE,
            events: Mutex::new(Vec::new()),
        });
        hub.set_script(Some(script.clone()));
        let mut sub = hub.subscribe(ChatInfo::default());
        let first = sub.frames.recv().await.unwrap();
        assert!(first.reset);
        assert!(
            first
                .ops
                .iter()
                .any(|op| matches!(op, Op::Create { node, .. } if *node == parts::FIRST_FREE))
        );
        let mut ahead = Vec::new();
        while ahead.len() < 4 {
            let frame = sub.frames.recv().await.unwrap();
            if frame.at > hub.now() {
                ahead.push(frame);
            }
        }
        assert!(ahead.windows(2).all(|w| w[0].at < w[1].at));

        hub.event(&sub.chat, Some(parts::FIRST_FREE), Event::Click);
        let cancel = loop {
            let frame = sub.frames.recv().await.unwrap();
            if frame.cancel_ahead {
                break frame;
            }
        };
        assert!(
            cancel
                .ops
                .iter()
                .any(|op| matches!(op, Op::Set { value, .. } if value == "clicked"))
        );
        assert_eq!(*script.events.lock().unwrap(), vec!["click".to_string()]);
    }
}

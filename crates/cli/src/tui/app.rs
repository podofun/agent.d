use std::cell::RefCell;
use std::collections::VecDeque;

use agentd_types::ApprovalRequest;
use anyhow::{Context, Result, anyhow};
use ratatui::text::Line;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::clipboard;
use super::commands;
use super::editor::Editor;
use super::presentation;
use super::search;
use super::ui::{self, Selection};
use crate::ws::{WsResponse, ws_call, ws_call_streaming_cancelable};
use ratatui::layout::Rect;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tab {
    Chat,
    All,
    Sessions,
    Runners,
    Skills,
    Actions,
    Services,
}

impl Tab {
    pub(super) const ALL: [Self; 7] = [
        Self::Chat,
        Self::All,
        Self::Sessions,
        Self::Runners,
        Self::Skills,
        Self::Actions,
        Self::Services,
    ];

    pub(super) fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    /// The browser tab after this one, wrapping around without leaving the browser.
    pub(super) fn next(self) -> Self {
        let tabs = &Self::ALL[1..];
        tabs[(self.index().max(1)) % tabs.len()]
    }

    /// The browser tab before this one, wrapping around without leaving the browser.
    pub(super) fn previous(self) -> Self {
        let tabs = &Self::ALL[1..];
        tabs[(self.index().max(1) + tabs.len() - 2) % tabs.len()]
    }
}

#[derive(Clone, Copy)]
pub(super) enum EntryKind {
    User,
    Agent,
    Tool,
    Info,
    Error,
    /// A change to the chat itself, such as its runner or approval mode.
    Setting,
}

pub(super) struct Entry {
    pub(super) kind: EntryKind,
    pub(super) text: String,
    pub(super) body: String,
    /// Render `text` as markdown instead of preformatted lines.
    pub(super) markdown: bool,
    pub(super) expanded: bool,
    /// On the first tool row of a run of tool rows: whether the run is shown
    /// one row per call instead of as a single "Used N actions" row.
    pub(super) group_open: bool,
    /// On a tool row: the arguments the call was given.
    pub(super) input: Option<Value>,
    /// On a tool row: the files the call changed.
    pub(super) changes: Vec<FileChange>,
    /// On a tool row: the call failed, or ran a program that exited nonzero.
    pub(super) failed: bool,
    /// On a tool row: the call's output as the tool returned it, when it is
    /// an object, so the row can lay its fields out instead of flat text.
    pub(super) output: Option<Value>,
    rendered: RefCell<Option<(usize, Vec<Line<'static>>)>>,
}

/// One file a tool call changed, as the daemon reports it on `tool_result`.
#[derive(Clone, Debug, serde::Deserialize)]
pub(super) struct FileChange {
    pub(super) path: String,
    /// `created`, `modified`, or `removed`.
    #[serde(default)]
    pub(super) operation: String,
    /// The unified diff, absent when the file is binary or too large.
    #[serde(default)]
    pub(super) diff: Option<String>,
    #[serde(default)]
    pub(super) binary: bool,
    #[serde(default)]
    pub(super) added: usize,
    #[serde(default)]
    pub(super) removed: usize,
}

/// A clickable tool row: one call, or a run of calls shown as one row,
/// named by the index of its first entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ToolRow {
    Entry(usize),
    Group(usize),
}

pub(super) enum Update {
    Delta(Value),
    RunFinished(Result<WsResponse>),
    ActionFinished {
        name: String,
        result: Result<WsResponse>,
    },
    Approval(Box<ApprovalRequest>),
    ControlLost(String),
    /// The daemon created the chat's session, with a note when it had to
    /// start without the folder `agentctl` runs in.
    SessionCreated {
        id: String,
        label: Option<String>,
        notice: Option<String>,
        /// The messages that started the chat, shown once it exists.
        messages: Vec<String>,
    },
    /// The session could not be created, so `messages` go back to the queue
    /// for the next message to carry.
    SessionFailed {
        error: anyhow::Error,
        messages: Vec<String>,
    },
    /// A fresh session list for the browser.
    Sessions(Vec<Value>),
}

pub(super) struct Decision {
    pub(super) id: u64,
    pub(super) verdict: &'static str,
}

const CANCELLING: &str = "Cancelling runner…";

const HELP: &str = "\
## Chat
- `Enter` sends, `Shift+Enter` or `Ctrl+J` adds a newline
- While the agent works, `Enter` queues the message; queued messages are sent together when the reply finishes
- `/` lists commands. `/runner`, `/call`, and `/open` open searchable lists
- `/rename NAME` labels the current chat
- `Ctrl+N`, `/new`, or `/clear` starts a new chat, `F5` refreshes daemon state
- `Ctrl+E` expands the latest tool call, or click a row. An expanded call shows the diff of each file it changed
- `Shift+Tab` or `/approval` chooses who answers approvals: you, the classifier, or nobody
- Drag with the mouse to select text; `Ctrl+C` copies it
- `PageUp`, `PageDown`, or the mouse wheel scroll
- `Esc` or `End` on an empty draft jumps to the bottom and clears the notice
- `Ctrl+C` copies a selection, cancels a run, or clears the draft; press it twice to quit

## Browse
- `Ctrl+P` opens the browser; type to search everywhere
- `←`/`→` or `Tab` cycle sections, `↑`/`↓` move and wrap around, `PageUp`/`PageDown`/`Home`/`End` jump
- `Enter` opens a row, `Esc` clears the search, then returns

## Editing
- `Shift+arrows` select, `Ctrl+V` pastes, `Ctrl+X` cuts
- `Alt+Backspace` and `Alt+Delete` remove words
- `Ctrl+A`, `Ctrl+W`, `Ctrl+U`, `Ctrl+K` work as in a shell

## Approvals
- `o` allows once, `f` allows always, `d` or `Esc` denies";

pub(super) struct App {
    pub(super) base: String,
    pub(super) timeout: u64,
    pub(super) tab: Tab,
    pub(super) runner: Option<String>,
    pub(super) session: Option<String>,
    pub(super) session_label: Option<String>,
    pub(super) input: Editor,
    pub(super) suggestion_index: usize,
    pub(super) suggestion_dismissed: Option<String>,
    pub(super) entries: Vec<Entry>,
    pub(super) streamed: String,
    pub(super) pending: bool,
    /// Messages sent while a reply was running, oldest first. Each is sent
    /// when the run before it finishes.
    pub(super) queue: VecDeque<String>,
    /// The queue failed to send. It waits for the next message, which
    /// carries it, instead of being retried on its own.
    pub(super) queue_held: bool,
    /// Who answers this chat's approval requests: `human`, `classifier`, or
    /// `nobody`. `None` follows the runner's default.
    pub(super) approval_mode: Option<&'static str>,
    /// Name of the folder `agentctl` runs in.
    pub(super) workdir: String,
    /// Git branch of that folder, if it is a repository.
    pub(super) branch: Option<String>,
    /// When Ctrl+C was last pressed with nothing else to do.
    pub(super) quit_pressed: Option<std::time::Instant>,
    cancel: Option<oneshot::Sender<()>>,
    started: Option<std::time::Instant>,
    pub(super) scroll: usize,
    /// Mouse selection over the conversation.
    pub(super) selection: Option<Selection>,
    /// The clickable conversation row under the mouse: its line, and what
    /// clicking it opens.
    pub(super) hover: Option<(usize, ToolRow)>,
    /// Where the mouse is while it drags a selection.
    pub(super) drag: Option<(u16, u16)>,
    /// The clickable row the left button went down on. A press there does
    /// not start a selection: releasing on the same row opens or closes it.
    pub(super) press: Option<ToolRow>,
    /// When a drag past the conversation's edge last scrolled it.
    pub(super) drag_scrolled: Option<std::time::Instant>,
    /// Terminal size from the last event, for selection math.
    pub(super) screen: Rect,
    /// Whether the terminal renders 24-bit colour.
    pub(super) truecolor: bool,
    /// When the welcome leaf was last clicked into a turn.
    pub(super) leaf_spin: Option<std::time::Instant>,
    unread: bool,
    pub(super) selected: [usize; 7],
    /// Browser search text, shared by every section.
    pub(super) query: String,
    pub(super) runners: Vec<Value>,
    pub(super) sessions: Vec<Value>,
    pub(super) actions: Vec<Value>,
    pub(super) services: Vec<Value>,
    pub(super) skills: Vec<Value>,
    pub(super) connected: bool,
    pub(super) notice: Option<String>,
    /// Why the last refresh failed, shown in an empty browser list.
    pub(super) load_error: Option<String>,
    pub(super) approval: Option<ApprovalRequest>,
    pub(super) approval_scroll: usize,
    approvals: VecDeque<ApprovalRequest>,
    decisions: mpsc::UnboundedSender<Decision>,
    pub(super) decisions_rx: Option<mpsc::UnboundedReceiver<Decision>>,
    pub(super) updates: mpsc::UnboundedSender<Update>,
    pub(super) received: mpsc::UnboundedReceiver<Update>,
}

impl App {
    pub(super) fn new(base: &str, timeout: u64, runner: Option<String>) -> Self {
        let (updates, received) = mpsc::unbounded_channel();
        let (decisions, decisions_rx) = mpsc::unbounded_channel();
        Self {
            base: base.to_owned(),
            timeout,
            tab: Tab::Chat,
            runner,
            session: None,
            session_label: None,
            input: Editor::default(),
            suggestion_index: 0,
            suggestion_dismissed: None,
            entries: Vec::new(),
            streamed: String::new(),
            pending: false,
            queue: VecDeque::new(),
            queue_held: false,
            approval_mode: None,
            workdir: std::env::current_dir()
                .ok()
                .and_then(|dir| {
                    dir.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
            branch: None,
            quit_pressed: None,
            cancel: None,
            started: None,
            scroll: 0,
            selection: None,
            hover: None,
            drag: None,
            press: None,
            drag_scrolled: None,
            screen: Rect::new(0, 0, 80, 24),
            truecolor: std::env::var("COLORTERM")
                .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit")),
            leaf_spin: None,
            unread: false,
            selected: [0; 7],
            query: String::new(),
            runners: Vec::new(),
            sessions: Vec::new(),
            actions: Vec::new(),
            services: Vec::new(),
            skills: Vec::new(),
            connected: false,
            notice: None,
            load_error: None,
            approval: None,
            approval_scroll: 0,
            approvals: VecDeque::new(),
            decisions,
            decisions_rx: Some(decisions_rx),
            updates,
            received,
        }
    }

    pub(super) fn push(&mut self, kind: EntryKind, text: impl Into<String>) {
        let markdown = matches!(kind, EntryKind::Agent);
        self.entries
            .push(Entry::new(kind, text.into(), String::new(), markdown));
        self.content_arrived();
    }

    pub(super) fn push_markdown(&mut self, kind: EntryKind, text: impl Into<String>) {
        self.entries
            .push(Entry::new(kind, text.into(), String::new(), true));
        self.content_arrived();
    }

    pub(super) fn push_tool(&mut self, title: impl Into<String>, body: impl Into<String>) {
        self.entries.push(Entry::new(
            EntryKind::Tool,
            title.into(),
            body.into(),
            false,
        ));
        self.content_arrived();
    }

    /// Fill in the row of a finished call with its input and output. Calls
    /// finish in the order they were made, so it is the first row of this
    /// reply with the same name and nothing filled in yet.
    fn tool_finished(&mut self, value: &Value) {
        let name = value["name"].as_str().unwrap_or("tool");
        let reply_start = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::User))
            .map_or(0, |index| index + 1);
        let index = (reply_start..self.entries.len()).find(|&index| {
            let entry = &self.entries[index];
            matches!(entry.kind, EntryKind::Tool)
                && entry.text == name
                && entry.input.is_none()
                && entry.body.is_empty()
        });
        let index = index.unwrap_or_else(|| {
            self.push_tool(name, "");
            self.entries.len() - 1
        });
        let output = match (&value["error"], &value["output"]) {
            (Value::String(error), _) => format!("failed: {error}"),
            (_, Value::String(text)) => presentation::tool_output(text),
            (_, output) => presentation::value(output),
        };
        let entry = &mut self.entries[index];
        entry.failed = call_failed(value);
        entry.output = output_object(&value["output"]);
        set_input(entry, &value["input"]);
        append_output(entry, &output);
        let changes = file_changes(&value["changes"]);
        if !changes.is_empty() {
            entry.set_changes(changes);
        }
    }

    fn content_arrived(&mut self) {
        if self.scroll > 0 {
            self.unread = true;
        }
    }

    /// True while something on screen moves and needs regular redraws.
    pub(super) fn animating(&self) -> bool {
        self.leaf_elapsed().is_some()
    }

    /// Start one turn of the welcome leaf, unless it is already turning.
    pub(super) fn spin_leaf(&mut self) {
        if !self.animating() {
            self.leaf_spin = Some(std::time::Instant::now());
        }
    }

    /// Time into the current leaf turn, or `None` when it is at rest.
    pub(super) fn leaf_elapsed(&self) -> Option<std::time::Duration> {
        self.leaf_spin
            .map(|started| started.elapsed())
            .filter(|elapsed| *elapsed < ui::LEAF_SPIN)
    }

    pub(super) fn has_unread(&self) -> bool {
        self.unread
    }

    pub(super) fn jump_to_bottom(&mut self) {
        self.scroll = 0;
        self.unread = false;
    }

    pub(super) fn started_at(&self) -> Option<std::time::Instant> {
        self.started
    }

    fn flush_streamed(&mut self) {
        let text = std::mem::take(&mut self.streamed);
        if !text.trim().is_empty() {
            self.push(EntryKind::Agent, text.trim_end());
        }
    }
    pub(super) fn suggestions(&self) -> Vec<commands::Suggestion> {
        if self.suggestion_dismissed.as_deref() == Some(self.input.text()) {
            Vec::new()
        } else {
            commands::matching(
                self.input.text(),
                &self.runners,
                &self.actions,
                &self.sessions,
            )
        }
    }

    pub(super) fn complete_suggestion(&mut self, index: usize) {
        if let Some(completion) = self
            .suggestions()
            .get(index)
            .and_then(|item| item.completion.clone())
        {
            self.input.set(completion);
            self.suggestion_index = 0;
            self.suggestion_dismissed = None;
        }
    }

    pub(super) fn input_changed(&mut self) {
        self.suggestion_index = 0;
        self.suggestion_dismissed = None;
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        daemon_call(&self.base, self.timeout, method, params).await
    }

    /// Whether a second Ctrl+C now quits.
    pub(super) fn quit_armed(&self) -> bool {
        self.quit_pressed
            .is_some_and(|pressed| pressed.elapsed() < std::time::Duration::from_millis(1500))
    }

    /// The model of the selected runner.
    pub(super) fn model(&self) -> Option<&str> {
        let runner = self.runner.as_deref()?;
        self.runners.iter().find(|item| item["name"] == runner)?["model"].as_str()
    }

    pub(super) async fn refresh(&mut self) {
        self.branch = current_branch().await;
        let mut errors = Vec::new();
        let (runners, sessions, actions, services, skills) = tokio::join!(
            ws_call(&self.base, self.timeout, "runners.list", Value::Null),
            ws_call(&self.base, self.timeout, "sessions.list", json!({})),
            ws_call(&self.base, self.timeout, "tools.list", Value::Null),
            ws_call(&self.base, self.timeout, "services.list", Value::Null),
            ws_call(&self.base, self.timeout, "skills.list", Value::Null),
        );
        for (method, target, result) in [
            ("runners.list", &mut self.runners, runners),
            ("sessions.list", &mut self.sessions, sessions),
            ("tools.list", &mut self.actions, actions),
            ("services.list", &mut self.services, services),
            ("skills.list", &mut self.skills, skills),
        ] {
            match result.and_then(response_result) {
                Ok(Value::Array(items)) => {
                    *target = items;
                    if method == "runners.list" {
                        self.connected = true;
                    }
                }
                Ok(_) => errors.push(format!("{method}: unexpected response")),
                Err(error) => {
                    if method == "runners.list" {
                        self.connected = false;
                    }
                    errors.push(format!("{method}: {error}"));
                }
            }
        }
        if self.runner.is_none() && self.runners.len() == 1 {
            self.runner = self.runners[0]["name"].as_str().map(str::to_owned);
        }
        for (tab, length) in [
            (Tab::Runners, self.runners.len()),
            (Tab::Sessions, self.sessions.len()),
            (Tab::Actions, self.actions.len()),
            (Tab::Services, self.services.len()),
            (Tab::Skills, self.skills.len()),
        ] {
            self.selected[tab.index()] = self.selected[tab.index()].min(length.saturating_sub(1));
        }
        self.load_error = if errors.is_empty() {
            None
        } else {
            Some(errors.join(" · "))
        };
    }

    /// The raw items behind one browser section.
    pub(super) fn section(&self, tab: Tab) -> &[Value] {
        match tab {
            Tab::Chat | Tab::All => &[],
            Tab::Runners => &self.runners,
            Tab::Sessions => &self.sessions,
            Tab::Actions => &self.actions,
            Tab::Services => &self.services,
            Tab::Skills => &self.skills,
        }
    }

    /// Browser rows for the current section and query, best matches first.
    pub(super) fn rows(&self) -> Vec<search::Row<'_>> {
        search::rows(self, self.tab, &self.query)
    }

    /// Text under the mouse selection at the current terminal width, if any.
    pub(super) fn selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let text = ui::selected_text(self, self.screen, selection);
        (!text.is_empty()).then_some(text)
    }

    /// Copy the mouse selection to the clipboard and release it.
    pub(super) fn copy_selection(&mut self) -> Option<Result<(), String>> {
        let text = self.selected_text()?;
        self.selection = None;
        Some(clipboard::copy(text))
    }

    /// Remember the terminal size; a new size re-wraps lines, so any
    /// selection no longer points at the right cells.
    pub(super) fn resized(&mut self, screen: Rect) {
        if self.screen != screen {
            self.screen = screen;
            self.selection = None;
        }
    }

    pub(super) fn selected_index(&self) -> usize {
        self.selected[self.tab.index()]
    }

    pub(super) fn set_query(&mut self, query: String) {
        self.query = query;
        self.selected = [0; 7];
    }

    /// Move the browser selection by `delta` rows. With `wrap`, stepping
    /// past either end continues from the other; otherwise it stops there.
    pub(super) fn move_selection(&mut self, delta: isize, wrap: bool) {
        let count = self.rows().len();
        let index = self.tab.index();
        if count == 0 {
            self.selected[index] = 0;
            return;
        }
        let current = self.selected[index].min(count - 1) as isize;
        let target = current + delta;
        self.selected[index] = if wrap && (target < 0 || target >= count as isize) {
            target.rem_euclid(count as isize) as usize
        } else {
            target.clamp(0, count as isize - 1) as usize
        };
    }

    pub(super) async fn activate_selected(&mut self) {
        let Some((section, item)) = self
            .rows()
            .get(self.selected_index())
            .map(|row| (row.section, row.value.clone()))
        else {
            return;
        };
        match section {
            Tab::Runners => {
                if self.pending {
                    self.fail("Wait for the current reply.");
                    return;
                }
                if let Some(name) = item["name"].as_str() {
                    self.switch_runner(name);
                }
                self.tab = Tab::Chat;
            }
            Tab::Sessions => {
                if self.pending {
                    self.fail("Wait for the current reply.");
                    return;
                }
                if let Some(id) = item["id"].as_str() {
                    if let Err(error) = self.open_session(id).await {
                        self.fail(error.to_string());
                    } else {
                        self.tab = Tab::Chat;
                    }
                }
            }
            Tab::Actions => {
                if let Some(name) = item.as_str() {
                    self.input.set(format!("/call {name} "));
                    self.tab = Tab::Chat;
                }
            }
            Tab::Skills => {
                if let Some(name) = item["name"].as_str() {
                    match self.call("skills.inspect", json!({ "name": name })).await {
                        Ok(value) => self.push(EntryKind::Info, presentation::value(&value)),
                        Err(error) => self.fail(error.to_string()),
                    }
                    self.tab = Tab::Chat;
                }
            }
            _ => {}
        }
    }

    pub(super) fn new_chat(&mut self) {
        self.queue.clear();
        self.queue_held = false;
        self.session = None;
        self.session_label = None;
        self.entries.clear();
        self.streamed.clear();
        self.scroll = 0;
        self.selection = None;
        self.unread = false;
        self.notice = None;
    }

    /// Use another runner from the next message on. The conversation goes
    /// on, and messages that waited for a runner go to this one.
    fn switch_runner(&mut self, name: &str) {
        if self.runner.as_deref() == Some(name) {
            return;
        }
        self.runner = Some(name.to_owned());
        self.queue_held = false;
        self.setting("runner", format!("Switched to {name}"));
    }

    pub(super) async fn open_session(&mut self, id: &str) -> Result<()> {
        let value = self.call("sessions.get", json!({ "id": id })).await?;
        let session = value["id"]
            .as_str()
            .context("session has no id")?
            .to_owned();
        if let Some(runner) = value["runner"].as_str() {
            self.runner = Some(runner.to_owned());
        }
        self.new_chat();
        self.session = Some(session);
        self.session_label = value["label"].as_str().map(str::to_owned);
        self.approval_mode = match value["approval"].as_str() {
            Some("human") => Some("human"),
            Some("classifier") => Some("classifier"),
            Some("nobody") => Some("nobody"),
            _ => None,
        };
        self.load_turns(&value["turns"]);
        Ok(())
    }

    pub(super) fn load_turns(&mut self, turns: &Value) {
        let mut awaiting_output = VecDeque::new();
        for turn in turns.as_array().into_iter().flatten() {
            match turn["role"].as_str() {
                Some("tool") => {
                    let output = presentation::tool_output(turn["content"].as_str().unwrap_or(""));
                    let index = match awaiting_output.pop_front() {
                        Some(index) => {
                            append_output(&mut self.entries[index], &output);
                            index
                        }
                        None => {
                            self.push_tool("Result", output);
                            self.entries.len() - 1
                        }
                    };
                    self.entries[index].output = output_object(&turn["content"]);
                    let changes = file_changes(&turn["changes"]);
                    if !changes.is_empty() {
                        self.entries[index].set_changes(changes);
                    }
                }
                role => {
                    let kind = match role {
                        Some("user") => EntryKind::User,
                        Some("assistant") => EntryKind::Agent,
                        _ => EntryKind::Info,
                    };
                    if let Some(content) = turn["content"]
                        .as_str()
                        .filter(|content| !content.is_empty())
                    {
                        self.push(kind, content);
                    }
                }
            }
            for call in turn["tool_calls"].as_array().into_iter().flatten() {
                awaiting_output.push_back(self.entries.len());
                self.push_tool(call["name"].as_str().unwrap_or("tool"), "");
                set_input(self.entries.last_mut().unwrap(), &call["arguments"]);
            }
        }
    }

    pub(super) async fn submit(&mut self, input: &str) -> Result<()> {
        let input = input.trim();
        if input.is_empty() {
            return Ok(());
        }
        if let Some(command) = input.strip_prefix('/') {
            return self.command(command).await;
        }
        // Wait for the reply, or for a runner, and send it then.
        if self.pending || self.runner.is_none() {
            self.queue.push_back(input.to_owned());
            const NO_RUNNER: &str = "No runner selected. Pick one with Ctrl+P.";
            let shown = self.entries.last().is_some_and(|entry| {
                matches!(entry.kind, EntryKind::Error) && entry.text == NO_RUNNER
            });
            if self.runner.is_none() && !shown {
                self.fail(NO_RUNNER);
            }
            return Ok(());
        }
        let mut messages: Vec<String> = self.queue.drain(..).collect();
        messages.push(input.to_owned());
        self.queue_held = false;
        self.send(messages).await
    }

    /// Send `messages` as one prompt, so a reply answers all of them. If
    /// they cannot be sent, they stay queued for the next message.
    async fn send(&mut self, messages: Vec<String>) -> Result<()> {
        let result = self.start_run(&messages).await;
        if result.is_err() {
            self.queue.extend(messages);
            self.queue_held = true;
        }
        result
    }

    /// What a new chat asks `sessions.create` for: its runner, the folder
    /// `agentctl` runs in, an approval mode if one is chosen, and a name
    /// taken from the first message.
    pub(super) fn session_params(&self, runner: &str, input: &str) -> Value {
        let mut params = json!({ "runner": runner, "label": self.title_for(input) });
        if let Ok(cwd) = std::env::current_dir() {
            params["cwd"] = json!(cwd.to_string_lossy());
        }
        if let Some(mode) = self.approval_mode {
            params["approval"] = json!(mode);
        }
        params
    }

    async fn start_run(&mut self, messages: &[String]) -> Result<()> {
        let input = messages.join("\n\n");
        let input = input.as_str();
        let runner = self.runner.clone().context("no runner")?;
        // A new chat's session is created in the background with the run, so
        // the chat stays live while the daemon asks about the folder.
        let new_session = self
            .session
            .is_none()
            .then(|| self.session_params(&runner, input));
        let mut params =
            json!({ "name": runner, "prompt": input, "session_id": self.session, "stream": true });
        if let Some(mode) = self.approval_mode {
            params["approval"] = json!(mode);
        }
        if new_session.is_none() {
            for message in messages {
                self.push(EntryKind::User, message.as_str());
            }
        }
        let first_messages = messages.to_vec();
        self.pending = true;
        self.started = Some(std::time::Instant::now());
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.cancel = Some(cancel_tx);
        self.streamed.clear();
        let base = self.base.clone();
        let timeout = self.timeout;
        let tx = self.updates.clone();
        tokio::spawn(async move {
            if let Some(session_params) = new_session {
                match create_session(&base, timeout, session_params).await {
                    Ok((id, label, notice)) => {
                        params["session_id"] = json!(id);
                        let _ = tx.send(Update::SessionCreated {
                            id,
                            label,
                            notice,
                            messages: first_messages,
                        });
                        if let Ok(Value::Array(items)) =
                            daemon_call(&base, timeout, "sessions.list", json!({})).await
                        {
                            let _ = tx.send(Update::Sessions(items));
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(Update::SessionFailed {
                            error,
                            messages: first_messages,
                        });
                        return;
                    }
                }
            }
            let result = ws_call_streaming_cancelable(
                &base,
                timeout,
                "runners.run",
                params,
                Some(cancel_rx),
                |delta| {
                    let _ = tx.send(Update::Delta(delta.clone()));
                },
            )
            .await;
            let _ = tx.send(Update::RunFinished(result));
        });
        Ok(())
    }

    /// `title(message)`, with ` 2`, ` 3`, … when another chat has it.
    fn title_for(&self, message: &str) -> String {
        let base = title(message);
        let taken = |name: &str| self.sessions.iter().any(|item| item["label"] == name);
        (1..)
            .map(|n| {
                if n == 1 {
                    base.clone()
                } else {
                    format!("{base} {n}")
                }
            })
            .find(|name| !taken(name))
            .unwrap_or(base)
    }

    async fn refresh_sessions(&mut self) {
        if let Ok(Value::Array(items)) = self.call("sessions.list", json!({})).await {
            self.sessions = items;
        }
    }

    async fn command(&mut self, command: &str) -> Result<()> {
        let (verb, argument) = command.split_once(' ').unwrap_or((command, ""));
        let argument = argument.trim();
        match verb {
            "help" => self.push_markdown(EntryKind::Info, HELP),
            "new" | "clear" => {
                if self.pending {
                    return Err(anyhow!("wait for the current reply"));
                }
                self.new_chat();
            }
            "approval" => {
                let mode = match argument {
                    "human" => Some("human"),
                    "classifier" => Some("classifier"),
                    "nobody" => Some("nobody"),
                    "default" => None,
                    _ => {
                        return Err(anyhow!("usage: /approval human|classifier|nobody|default"));
                    }
                };
                self.set_approval_mode(mode).await?;
            }
            "runner" => {
                if self.pending {
                    return Err(anyhow!("wait for the current reply"));
                }
                if !self.runners.iter().any(|item| item["name"] == argument) {
                    return Err(anyhow!("runner `{argument}` is not registered"));
                }
                self.switch_runner(argument);
            }
            "rename" => {
                let session = self
                    .session
                    .clone()
                    .context("start a chat before renaming it")?;
                if argument.is_empty() {
                    return Err(anyhow!("usage: /rename NAME"));
                }
                let renamed = self
                    .call(
                        "sessions.rename",
                        json!({ "id": session, "label": argument }),
                    )
                    .await?;
                self.session_label = renamed["label"].as_str().map(str::to_owned);
                self.setting("name", format!("Renamed to {argument}"));
                self.refresh_sessions().await;
            }
            "open" => {
                if self.pending {
                    return Err(anyhow!("wait for the current reply"));
                }
                if argument.is_empty() {
                    return Err(anyhow!("usage: /open ID"));
                }
                self.open_session(argument).await?;
            }
            "call" => {
                let (name, args) = argument.split_once(' ').unwrap_or((argument, "{}"));
                if name.is_empty() {
                    return Err(anyhow!("usage: /call ACTION JSON"));
                }
                let args: Value = serde_json::from_str(args).context("invalid action JSON")?;
                self.notice = Some(format!("Running {name}…"));
                let base = self.base.clone();
                let timeout = self.timeout;
                let tx = self.updates.clone();
                let params = json!({ "name": name, "args": args });
                let name = name.to_owned();
                tokio::spawn(async move {
                    let _ = tx.send(Update::ActionFinished {
                        name,
                        result: ws_call(&base, timeout, "actions.call", params).await,
                    });
                });
            }
            _ => return Err(anyhow!("unknown command `/{verb}`; type /help")),
        }
        Ok(())
    }

    pub(super) fn apply(&mut self, update: Update) {
        match update {
            Update::Delta(value) => match value["type"].as_str() {
                Some("text_delta") => {
                    if let Some(text) = value["text"].as_str() {
                        self.streamed.push_str(text);
                        self.content_arrived();
                    }
                }
                Some("turn_end") => self.flush_streamed(),
                Some("tool_call") => {
                    self.flush_streamed();
                    self.push_tool(value["name"].as_str().unwrap_or("tool"), "");
                }
                Some("tool_result") => self.tool_finished(&value),
                _ => {}
            },
            Update::RunFinished(result) => {
                self.pending = false;
                self.cancel = None;
                self.started = None;
                if self.notice.as_deref() == Some(CANCELLING) {
                    self.notice = None;
                }
                self.flush_streamed();
                match result.and_then(response_result) {
                    Ok(value) => {
                        let text = value["text"].as_str().unwrap_or("").trim_end();
                        let already_shown = self.entries.last().is_some_and(|entry| {
                            matches!(entry.kind, EntryKind::Agent) && entry.text == text
                        });
                        if !text.is_empty() && !already_shown {
                            self.push(EntryKind::Agent, text);
                        }
                    }
                    Err(error) => {
                        self.push(EntryKind::Error, error.to_string());
                        self.queue_held = true;
                    }
                }
            }
            Update::SessionCreated {
                id,
                label,
                notice,
                messages,
            } => {
                self.session = Some(id);
                self.session_label = label;
                if notice.is_some() {
                    self.notice = notice;
                }
                for message in &messages {
                    self.push(EntryKind::User, message.as_str());
                }
            }
            Update::SessionFailed { error, messages } => {
                self.pending = false;
                self.cancel = None;
                self.started = None;
                self.fail(error.to_string());
                self.queue.extend(messages);
                self.queue_held = true;
            }
            Update::Sessions(items) => self.sessions = items,
            Update::ActionFinished { name, result } => {
                self.notice = None;
                match result.and_then(response_result) {
                    Ok(value) => self.push_tool(name, presentation::action_result(&value)),
                    Err(error) => self.push(EntryKind::Error, format!("{name}: {error}")),
                }
            }
            Update::Approval(request) => {
                let request = *request;
                if self.approval.is_none() {
                    self.approval = Some(request);
                    self.approval_scroll = 0;
                } else {
                    self.approvals.push_back(request);
                }
            }
            Update::ControlLost(error) => {
                self.approval = None;
                self.approvals.clear();
                self.fail(format!("Approvals unavailable: {error}"));
            }
        }
        if self.scroll == 0 {
            self.unread = false;
        }
    }

    pub(super) fn decide(&mut self, verdict: &'static str) {
        let Some(request) = self.approval.take() else {
            return;
        };
        let id = request.id;
        if self.decisions.send(Decision { id, verdict }).is_err() {
            self.fail("Approval connection closed");
        } else {
            let outcome = match verdict {
                "allow_once" => "Allowed once",
                "allow_forever" => "Always allowed",
                _ => "Denied",
            };
            self.push(EntryKind::Info, format!("{} · {outcome}", request.action));
        }
        self.approval = self.approvals.pop_front();
        self.approval_scroll = 0;
    }

    /// Whether the queue goes out now: a runner is set, no reply is running,
    /// and the queue did not just fail.
    pub(super) fn queue_ready(&self) -> bool {
        !self.queue.is_empty() && !self.pending && !self.queue_held && self.runner.is_some()
    }

    /// Send every queued message as one prompt, once a reply finished or a
    /// runner is set.
    pub(super) async fn send_queued(&mut self) {
        if !self.queue_ready() {
            return;
        }
        let messages: Vec<String> = self.queue.drain(..).collect();
        if let Err(error) = self.send(messages).await {
            self.fail(error.to_string());
        }
    }

    /// Set who answers this chat's approval requests. Stored on the chat's
    /// session, so it applies to the next reply and survives reopening.
    pub(super) async fn set_approval_mode(&mut self, mode: Option<&'static str>) -> Result<()> {
        if let Some(session) = &self.session {
            self.call(
                "sessions.set_approval",
                json!({ "id": session, "approval": mode }),
            )
            .await?;
        }
        self.approval_mode = mode;
        let label = match mode {
            Some("classifier") => "Auto mode",
            Some("nobody") => "No approvals",
            Some(_) => "Manual approval",
            None => "Default approval",
        };
        self.setting("approval", format!("Approval: {label}"));
        Ok(())
    }

    /// Show an error in the conversation.
    pub(super) fn fail(&mut self, message: impl Into<String>) {
        self.push(EntryKind::Error, message.into());
    }

    /// Note a chat setting change in the conversation. Changing the same
    /// setting again replaces the note instead of adding another. The
    /// setting's name is kept in `body`, which a note does not show.
    pub(super) fn setting(&mut self, name: &str, text: String) {
        if let Some(last) = self.entries.last_mut()
            && matches!(last.kind, EntryKind::Setting)
            && last.body == name
        {
            last.text = text;
            last.rendered.get_mut().take();
            return;
        }
        self.push(EntryKind::Setting, text);
        if let Some(last) = self.entries.last_mut() {
            last.body = name.to_owned();
        }
    }

    /// Shift+Tab: human, then classifier, then nobody, then back.
    pub(super) async fn cycle_approval_mode(&mut self) -> Result<()> {
        let next = match self.approval_mode {
            None | Some("human") => "classifier",
            Some("classifier") => "nobody",
            _ => "human",
        };
        self.set_approval_mode(Some(next)).await
    }

    pub(super) fn cancel_run(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
            self.notice = Some(CANCELLING.into());
        }
    }

    /// The run of tool rows that `index` belongs to, as `start..end`.
    pub(super) fn tool_run(&self, index: usize) -> std::ops::Range<usize> {
        let is_tool = |entry: &Entry| matches!(entry.kind, EntryKind::Tool);
        let start = self.entries[..index]
            .iter()
            .rposition(|entry| !is_tool(entry))
            .map_or(0, |before| before + 1);
        let end = self.entries[index..]
            .iter()
            .position(|entry| !is_tool(entry))
            .map_or(self.entries.len(), |after| index + after);
        start..end
    }

    /// What Ctrl+E toggles: the latest closed group of tool rows, or else
    /// the latest tool row with output.
    pub(super) fn latest_tool_row(&self) -> Option<ToolRow> {
        let last = self
            .entries
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::Tool))?;
        let run = self.tool_run(last);
        if run.len() >= 2 && !self.entries[run.start].group_open {
            return Some(ToolRow::Group(run.start));
        }
        self.entries
            .iter()
            .rposition(Entry::is_expandable_tool)
            .map(ToolRow::Entry)
    }
}

impl Entry {
    fn new(kind: EntryKind, text: String, body: String, markdown: bool) -> Self {
        Self {
            kind,
            text,
            body,
            markdown,
            expanded: false,
            group_open: false,
            input: None,
            changes: Vec::new(),
            failed: false,
            output: None,
            rendered: RefCell::new(None),
        }
    }

    pub(super) fn is_expandable_tool(&self) -> bool {
        matches!(self.kind, EntryKind::Tool)
            && (!self.body.is_empty() || self.input.is_some() || !self.changes.is_empty())
    }

    pub(super) fn set_changes(&mut self, changes: Vec<FileChange>) {
        self.changes = changes;
        self.rendered.get_mut().take();
    }

    pub(super) fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
        self.rendered.get_mut().take();
    }

    /// Rendered lines for `width`, computed once and reused until the entry changes.
    pub(super) fn lines(
        &self,
        width: usize,
        render: impl FnOnce() -> Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        let mut cache = self.rendered.borrow_mut();
        match cache.as_ref() {
            Some((cached_width, lines)) if *cached_width == width => lines.clone(),
            _ => {
                let lines = render();
                *cache = Some((width, lines.clone()));
                lines
            }
        }
    }
}

/// A `tool_result` that carries an error, or output with a nonzero
/// `exit_code`, whether as an object or as JSON text.
fn call_failed(value: &Value) -> bool {
    if value["error"].is_string() {
        return true;
    }
    let parsed;
    let output = match &value["output"] {
        Value::String(text) => {
            parsed = serde_json::from_str(text).unwrap_or(Value::Null);
            &parsed
        }
        output => output,
    };
    output["exit_code"].as_i64().is_some_and(|code| code != 0)
}

/// A call's output when it is an object, whether sent as one or as JSON text.
/// The files a call changed, from a tool result or a saved tool turn. A
/// change that does not parse is skipped, so the rest still show.
fn file_changes(value: &Value) -> Vec<FileChange> {
    value.as_array().map_or_else(Vec::new, |changes| {
        changes
            .iter()
            .filter_map(|change| serde_json::from_value(change.clone()).ok())
            .collect()
    })
}

fn output_object(output: &Value) -> Option<Value> {
    let parsed = match output {
        Value::String(text) => serde_json::from_str(text).ok()?,
        output => output.clone(),
    };
    parsed.is_object().then_some(parsed)
}

/// Keep a call's arguments, unless it had none.
fn set_input(entry: &mut Entry, input: &Value) {
    let empty = input.is_null() || input.as_object().is_some_and(serde_json::Map::is_empty);
    entry.input = (!empty).then(|| input.clone());
    entry.rendered.get_mut().take();
}

fn append_output(entry: &mut Entry, output: &str) {
    if !entry.body.is_empty() && !output.is_empty() {
        entry.body.push_str("\n\n");
    }
    entry.body.push_str(output);
    entry.rendered.get_mut().take();
}

/// A short chat name from its first message, unique among known chats.
fn title(message: &str) -> String {
    const MAX: usize = 40;
    let mut title = String::new();
    let mut length = 0;
    for word in message.split_whitespace() {
        let words = word.chars().count();
        if length > 0 && length + 1 + words > MAX {
            break;
        }
        if length > 0 {
            title.push(' ');
            length += 1;
        }
        title.push_str(word);
        length += words;
    }
    title.chars().take(MAX).collect()
}

async fn current_branch() -> Option<String> {
    let output = tokio::task::spawn_blocking(|| {
        std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .stderr(std::process::Stdio::null())
            .output()
    })
    .await
    .ok()?
    .ok()?;
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !branch.is_empty()).then_some(branch)
}

/// One request to the daemon, with its refusal kept as a [`DaemonError`].
async fn daemon_call(base: &str, timeout: u64, method: &str, params: Value) -> Result<Value> {
    response_result(ws_call(base, timeout, method, params).await?)
}

/// Create a chat's session. A folder the daemon refuses, or a taken name, is
/// not worth failing the message over, so it retries without them. Returns
/// the id, the label, and a note when the chat works without its folder.
async fn create_session(
    base: &str,
    timeout: u64,
    mut params: Value,
) -> Result<(String, Option<String>, Option<String>)> {
    let mut notice = None;
    let mut created = daemon_call(base, timeout, "sessions.create", params.clone()).await;
    if let Err(error) = &created
        && error.to_string().contains("already exists")
    {
        params.as_object_mut().map(|map| map.remove("label"));
        created = daemon_call(base, timeout, "sessions.create", params.clone()).await;
    }
    if let Err(error) = &created
        && folder_refused(error)
    {
        notice = Some(format!(
            "Working without a folder: {}",
            error.to_string().lines().next().unwrap_or_default()
        ));
        params.as_object_mut().map(|map| map.remove("cwd"));
        created = daemon_call(base, timeout, "sessions.create", params).await;
    }
    let created = created?;
    let id = created["id"]
        .as_str()
        .context("session has no id")?
        .to_owned();
    Ok((id, created["label"].as_str().map(str::to_owned), notice))
}

/// A refusal from the daemon, keeping its code so callers can react to a
/// specific one. It reads the same as before: the message, then the tip.
#[derive(Debug)]
pub(super) struct DaemonError {
    pub(super) code: Option<String>,
    pub(super) message: String,
    pub(super) tip: Option<String>,
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.tip {
            Some(tip) => write!(f, "{}\n{tip}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for DaemonError {}

/// Whether the daemon refused the folder a new chat asked to work in, so the
/// chat can start without one.
pub(super) fn folder_refused(error: &anyhow::Error) -> bool {
    error.downcast_ref::<DaemonError>().is_some_and(|error| {
        matches!(
            error.code.as_deref(),
            Some("session_folder_denied" | "session_folder_not_allowed")
        )
    })
}

fn response_result(response: WsResponse) -> Result<Value> {
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        Err(DaemonError {
            code: response.code,
            message: response.error.unwrap_or_else(|| "unknown error".into()),
            tip: response.tip,
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn sending_a_message_never_waits_for_the_daemon() {
        // A daemon that accepts the connection and never answers, like one
        // waiting for the user to answer a folder question in this chat.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut app = App::new(&base, 30_000, Some("coder".into()));
        let started = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            app.start_run(&["hello".into()]),
        )
        .await;
        assert!(started.is_ok(), "start_run waited for the daemon");
        assert!(app.pending);
        drop(listener);
    }

    #[test]
    fn a_created_session_is_adopted_with_its_notice() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, Some("coder".into()));
        app.apply(Update::SessionCreated {
            id: "s1".into(),
            label: Some("hello".into()),
            notice: Some("Working without a folder: no".into()),
            messages: vec!["hello".into()],
        });
        assert!(
            app.entries
                .iter()
                .any(|e| matches!(e.kind, EntryKind::User) && e.text == "hello")
        );
        assert_eq!(app.session.as_deref(), Some("s1"));
        assert_eq!(app.session_label.as_deref(), Some("hello"));
        assert_eq!(app.notice.as_deref(), Some("Working without a folder: no"));
    }

    #[test]
    fn new_chats_send_their_folder() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, Some("coder".into()));
        let params = app.session_params("coder", "hello");
        let here = std::env::current_dir().unwrap();
        assert_eq!(params["cwd"], here.to_string_lossy().as_ref());
        assert_eq!(params["runner"], "coder");
        app.approval_mode = Some("human");
        assert_eq!(app.session_params("coder", "hello")["approval"], "human");
    }

    #[test]
    fn only_folder_refusals_fall_back_to_no_folder() {
        let refused = |code: &str| {
            anyhow::Error::new(DaemonError {
                code: Some(code.into()),
                message: "no".into(),
                tip: None,
            })
        };
        assert!(folder_refused(&refused("session_folder_denied")));
        assert!(folder_refused(&refused("session_folder_not_allowed")));
        assert!(!folder_refused(&refused("session_label_taken")));
        assert!(!folder_refused(&anyhow::anyhow!("session folder")));
        assert_eq!(refused("x").to_string(), "no");
    }

    #[test]
    fn setting_changes_show_in_the_conversation_once_each() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.setting("approval", "Approval: Auto mode".into());
        app.setting("approval", "Approval: No approvals".into());
        app.setting("runner", "Switched to coder".into());
        let texts: Vec<_> = app
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(texts, ["Approval: No approvals", "Switched to coder"]);
        assert!(matches!(app.entries[0].kind, EntryKind::Setting));
    }

    #[test]
    fn chats_are_named_after_their_first_message() {
        assert_eq!(title("  fix the\nlogin bug  "), "fix the login bug");
        assert_eq!(title(&"word ".repeat(30)).chars().count(), 39);
        assert_eq!(title(&"élan ".repeat(30)).chars().count(), 39);
        assert_eq!(title(&"é".repeat(60)).chars().count(), 40);
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.sessions = vec![json!({ "label": "hello" }), json!({ "label": "hello 2" })];
        assert_eq!(app.title_for("hello"), "hello 3");
        assert_eq!(app.title_for("other"), "other");
    }

    fn delta(value: Value) -> Update {
        Update::Delta(value)
    }

    fn finished(text: &str) -> Update {
        Update::RunFinished(Ok(WsResponse {
            id: 1,
            ok: true,
            result: Some(json!({ "text": text })),
            error: None,
            code: None,
            tip: None,
            trace: None,
        }))
    }

    #[test]
    fn failed_response_shows_message_and_tip_without_code() {
        let response = WsResponse {
            id: 1,
            ok: false,
            result: None,
            error: Some("runner `ghost` not registered".into()),
            code: Some("runner_not_found".into()),
            tip: Some("Run `agentctl runner ls` to list runners".into()),
            trace: None,
        };
        let error = response_result(response).unwrap_err().to_string();
        assert_eq!(
            error,
            "runner `ghost` not registered\nRun `agentctl runner ls` to list runners"
        );
    }

    #[test]
    fn text_before_a_tool_call_stays_above_it() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.pending = true;
        app.apply(delta(
            json!({ "type": "text_delta", "text": "Looking at " }),
        ));
        app.apply(delta(json!({ "type": "text_delta", "text": "the diff." })));
        app.apply(delta(json!({ "type": "tool_call", "name": "git.diff" })));
        app.apply(delta(json!({ "type": "turn_end" })));
        assert_eq!(app.entries.len(), 2);
        assert!(matches!(app.entries[0].kind, EntryKind::Agent));
        assert_eq!(app.entries[0].text, "Looking at the diff.");
        assert_eq!(app.entries[1].text, "git.diff");
        assert!(app.streamed.is_empty());
    }

    #[test]
    fn run_finish_does_not_duplicate_the_final_reply() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.pending = true;
        app.apply(delta(json!({ "type": "text_delta", "text": "All good." })));
        app.apply(delta(json!({ "type": "turn_end" })));
        app.apply(finished("All good."));
        assert_eq!(app.entries.len(), 1);
        assert!(!app.pending);
    }

    #[test]
    fn run_finish_flushes_text_without_a_turn_end() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.pending = true;
        app.apply(delta(json!({ "type": "text_delta", "text": "Partial" })));
        app.apply(finished("Partial"));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].text, "Partial");
    }

    #[test]
    fn run_finish_adds_a_final_reply_that_was_never_streamed() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.pending = true;
        app.apply(finished("Done."));
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].text, "Done.");
    }

    #[test]
    fn reopened_session_pairs_each_output_with_its_own_call() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.load_turns(&json!([
            { "role": "assistant", "content": "", "tool_calls": [
                { "name": "git.status", "arguments": {} },
                { "name": "git.diff", "arguments": { "staged": true } }
            ] },
            { "role": "tool", "content": "\"On branch main\"" },
            { "role": "tool", "content": "\"+line\"" }
        ]));
        assert_eq!(app.entries[0].body, "On branch main");
        assert!(
            app.entries[0].input.is_none(),
            "empty arguments are dropped"
        );
        assert_eq!(app.entries[1].body, "+line");
        assert_eq!(app.entries[1].input, Some(json!({ "staged": true })));
    }

    #[test]
    fn a_reopened_chat_shows_the_files_each_call_changed() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.load_turns(&json!([
            { "role": "assistant", "content": "", "tool_calls": [
                { "name": "files.write", "arguments": { "path": "a.txt", "content": "hi\n" } }
            ] },
            { "role": "tool", "content": "{\"path\":\"a.txt\"}", "changes": [
                { "path": "a.txt", "operation": "created", "diff": "@@ -0,0 +1 @@\n+hi\n",
                  "binary": false, "added": 1, "removed": 0 }
            ] }
        ]));
        assert_eq!(app.entries[0].changes.len(), 1);
        assert_eq!(app.entries[0].changes[0].path, "a.txt");
    }

    #[test]
    fn a_finished_call_fills_its_own_row() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "go");
        app.apply(delta(json!({ "type": "tool_call", "name": "fs.read" })));
        app.apply(delta(json!({ "type": "tool_call", "name": "fs.read" })));
        app.apply(delta(json!({
            "type": "tool_result", "name": "fs.read",
            "input": { "path": "/a.rs" }, "output": "first"
        })));
        app.apply(delta(json!({
            "type": "tool_result", "name": "fs.read",
            "input": { "path": "/b.rs" }, "error": "denied"
        })));
        assert_eq!(app.entries[1].input, Some(json!({ "path": "/a.rs" })));
        assert_eq!(app.entries[1].body, "first");
        assert_eq!(app.entries[2].input, Some(json!({ "path": "/b.rs" })));
        assert_eq!(app.entries[2].body, "failed: denied");
    }

    #[test]
    fn tool_result_changes_are_kept() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "go");
        app.apply(delta(json!({ "type": "tool_call", "name": "fs.write" })));
        app.apply(delta(json!({
            "type": "tool_result", "name": "fs.write",
            "input": { "path": "src/a.rs" }, "output": "ok",
            "changes": [{
                "path": "src/a.rs", "operation": "modified", "revision": 3,
                "diff": "@@ -1 +1,2 @@\n-a\n+b\n+c\n",
                "binary": false, "added": 2, "removed": 1
            }]
        })));
        let entry = &app.entries[1];
        assert_eq!(entry.changes.len(), 1);
        assert_eq!(entry.changes[0].path, "src/a.rs");
        assert_eq!(entry.changes[0].operation, "modified");
        assert_eq!(entry.changes[0].added, 2);
        assert_eq!(entry.changes[0].removed, 1);
    }

    #[test]
    fn a_call_that_only_changed_files_can_be_opened() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.apply(delta(json!({
            "type": "tool_result", "name": "shell",
            "changes": [{ "path": "a.txt", "operation": "created" }, { "bad": true }]
        })));
        let entry = &app.entries[0];
        assert_eq!(entry.changes.len(), 1, "a malformed change is skipped");
        assert_eq!(entry.changes[0].operation, "created");
        assert!(entry.is_expandable_tool());
    }

    #[test]
    fn finishing_a_cancelled_run_clears_the_cancelling_notice() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.pending = true;
        app.cancel = Some(oneshot::channel().0);
        app.cancel_run();
        assert!(app.notice.is_some());
        app.apply(finished(""));
        assert!(app.notice.is_none());
    }

    #[test]
    fn new_content_keeps_the_scroll_position_and_marks_unread() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::Agent, "old");
        app.scroll = 5;
        app.apply(delta(json!({ "type": "tool_call", "name": "git.diff" })));
        assert_eq!(app.scroll, 5);
        assert!(app.has_unread());
        app.jump_to_bottom();
        assert_eq!(app.scroll, 0);
        assert!(!app.has_unread());
    }

    #[test]
    fn reopened_session_pairs_tool_output_with_its_call() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.load_turns(&json!([
            { "role": "user", "content": "status?" },
            { "role": "assistant", "content": "", "tool_calls": [{ "name": "git.status", "arguments": {} }] },
            { "role": "tool", "content": "" },
            { "role": "assistant", "content": "Clean." }
        ]));
        assert_eq!(app.entries.len(), 3);
        assert_eq!(app.entries[1].text, "git.status");
        assert!(!app.entries[1].is_expandable_tool());
        assert_eq!(app.entries[2].text, "Clean.");
    }

    #[test]
    fn tool_entries_keep_title_and_body_apart() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("git.status", "");
        assert!(!app.entries[0].is_expandable_tool());
        app.push_tool("git.diff", "Path: README.md");
        assert!(app.entries[1].is_expandable_tool());
        assert_eq!(app.entries[1].text, "git.diff");
        assert_eq!(app.entries[1].body, "Path: README.md");
    }

    #[tokio::test]
    async fn help_lists_bindings_one_per_line() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.submit("/help").await.unwrap();
        let text = &app.entries[0].text;
        assert!(matches!(app.entries[0].kind, EntryKind::Info));
        assert!(app.entries[0].markdown);
        assert!(text.lines().count() >= 10);
        assert!(text.contains("- `Ctrl+P`"));
        assert!(text.contains("- `Esc`"));
    }

    #[test]
    fn entry_lines_are_rendered_once_per_width_and_again_after_toggle() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("git.diff", "body");
        let renders = std::cell::Cell::new(0);
        let render = || {
            renders.set(renders.get() + 1);
            vec![ratatui::text::Line::raw("x")]
        };
        app.entries[0].lines(40, render);
        app.entries[0].lines(40, render);
        assert_eq!(renders.get(), 1);
        app.entries[0].lines(20, render);
        assert_eq!(renders.get(), 2);
        app.entries[0].set_expanded(true);
        app.entries[0].lines(20, render);
        assert_eq!(renders.get(), 3);
    }

    #[tokio::test]
    async fn rename_needs_an_open_session_and_a_name() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let error = app.submit("/rename work").await.unwrap_err().to_string();
        assert!(error.contains("start a chat"), "{error}");
        app.session = Some("abc".into());
        let error = app.submit("/rename").await.unwrap_err().to_string();
        assert!(error.contains("usage: /rename"), "{error}");
    }

    #[tokio::test]
    async fn messages_sent_while_working_wait_and_stay_after_a_failure() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, Some("chat".into()));
        app.pending = true;
        app.submit("second").await.unwrap();
        app.submit("third").await.unwrap();
        assert_eq!(app.queue, ["second", "third"]);
        assert!(!app.queue_ready(), "nothing is sent while working");

        app.apply(Update::RunFinished(Err(anyhow!("the run failed"))));
        assert_eq!(app.queue, ["second", "third"], "a failed reply keeps them");
        assert!(app.input.is_empty());
        assert!(!app.queue_ready(), "they wait for the next message");
    }

    #[tokio::test]
    async fn queued_messages_go_as_one_prompt_and_fail_once() {
        // Nothing listens on this port, so creating the session fails.
        let mut app = App::new("http://127.0.0.1:9", 1000, Some("coder".into()));
        app.queue.extend(["hey".to_owned(), "there".to_owned()]);
        app.send_queued().await;
        // The session is created in the background; its failure arrives as an update.
        let update = app.received.recv().await.unwrap();
        app.apply(update);
        app.send_queued().await;
        let errors = app
            .entries
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Error))
            .count();
        assert_eq!(errors, 1);
        assert_eq!(app.queue, ["hey", "there"], "they stay in the conversation");
        assert!(app.input.is_empty(), "nothing goes back to the draft");

        // The next message carries them, and fails as one.
        app.submit("again").await.unwrap();
        let update = app.received.recv().await.unwrap();
        app.apply(update);
        assert_eq!(app.queue, ["hey", "there", "again"]);
    }

    #[tokio::test]
    async fn messages_without_a_runner_wait_for_one() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.runners = vec![json!({ "name": "coder" })];
        app.submit("hello").await.unwrap();
        app.submit("and this").await.unwrap();
        assert_eq!(app.queue, ["hello", "and this"]);
        assert!(!app.queue_ready(), "nothing is sent without a runner");
        let errors: Vec<_> = app
            .entries
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Error))
            .map(|entry| entry.text.as_str())
            .collect();
        assert_eq!(
            errors,
            ["No runner selected. Pick one with Ctrl+P."],
            "shown once"
        );

        app.submit("/runner coder").await.unwrap();
        assert_eq!(
            app.queue,
            ["hello", "and this"],
            "picking a runner keeps them"
        );
        assert!(app.queue_ready());
        assert_eq!(app.entries.last().unwrap().text, "Switched to coder");
        assert_eq!(app.entries.len(), 2, "the conversation goes on");
    }

    #[tokio::test]
    async fn clear_starts_a_new_chat_and_approval_needs_a_valid_mode() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "hello");
        app.submit("/clear").await.unwrap();
        assert!(app.entries.is_empty());

        app.submit("/approval nobody").await.unwrap();
        assert_eq!(app.approval_mode, Some("nobody"));
        app.cycle_approval_mode().await.unwrap();
        assert_eq!(app.approval_mode, Some("human"));
        app.cycle_approval_mode().await.unwrap();
        assert_eq!(app.approval_mode, Some("classifier"));
        app.submit("/approval default").await.unwrap();
        assert_eq!(app.approval_mode, None);
        assert!(app.submit("/approval sometimes").await.is_err());
    }
}

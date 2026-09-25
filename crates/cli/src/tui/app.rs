use std::cell::RefCell;
use std::collections::VecDeque;

use agentd_types::ApprovalRequest;
use anyhow::{Context, Result, anyhow};
use ratatui::text::Line;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::commands;
use super::editor::Editor;
use super::presentation;
use super::search;
use crate::ws::{WsResponse, ws_call, ws_call_streaming_cancelable};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Tab {
    Chat,
    All,
    Runners,
    Sessions,
    Actions,
    Services,
    Skills,
}

impl Tab {
    pub(super) const ALL: [Self; 7] = [
        Self::Chat,
        Self::All,
        Self::Runners,
        Self::Sessions,
        Self::Actions,
        Self::Services,
        Self::Skills,
    ];

    pub(super) fn index(self) -> usize {
        Self::ALL.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    pub(super) fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    pub(super) fn previous(self) -> Self {
        Self::ALL[(self.index() + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Clone, Copy)]
pub(super) enum EntryKind {
    User,
    Agent,
    Tool,
    Info,
    Error,
}

pub(super) struct Entry {
    pub(super) kind: EntryKind,
    pub(super) text: String,
    pub(super) body: String,
    /// Render `text` as markdown instead of preformatted lines.
    pub(super) markdown: bool,
    pub(super) expanded: bool,
    rendered: RefCell<Option<(usize, Vec<Line<'static>>)>>,
}

pub(super) enum Update {
    Delta(Value),
    RunFinished(Result<WsResponse>),
    ActionFinished {
        name: String,
        result: Result<WsResponse>,
    },
    Approval(ApprovalRequest),
    ControlLost(String),
}

pub(super) struct Decision {
    pub(super) id: u64,
    pub(super) verdict: &'static str,
}

const CANCELLING: &str = "Cancelling runner…";

const HELP: &str = "\
## Chat
- `Enter` sends, `Shift+Enter` or `Ctrl+J` adds a newline
- `/` lists commands. `/runner`, `/call`, and `/open` open searchable lists
- `/rename NAME` labels the current chat
- `Ctrl+N` starts a new chat, `F5` refreshes daemon state
- `Ctrl+E` or a click expands the latest tool output
- `PageUp`, `PageDown`, or the mouse wheel scroll
- `Esc` or `End` on an empty draft jumps to the bottom and clears the notice
- `Ctrl+C` copies a selection, cancels a run, clears the draft, or quits

## Browse
- `Ctrl+P` opens the browser; type to search everywhere
- `Tab` changes section, `Enter` opens a row, `Esc` clears the search, then returns

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
    cancel: Option<oneshot::Sender<()>>,
    started: Option<std::time::Instant>,
    pub(super) scroll: usize,
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
            cancel: None,
            started: None,
            scroll: 0,
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

    fn content_arrived(&mut self) {
        if self.scroll > 0 {
            self.unread = true;
        }
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
        response_result(ws_call(&self.base, self.timeout, method, params).await?)
    }

    pub(super) async fn refresh(&mut self) {
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
        self.notice = if errors.is_empty() {
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

    pub(super) fn selected_index(&self) -> usize {
        self.selected[self.tab.index()]
    }

    pub(super) fn set_query(&mut self, query: String) {
        self.query = query;
        self.selected = [0; 7];
    }

    pub(super) fn select_previous(&mut self) {
        let index = self.tab.index();
        self.selected[index] = self.selected[index].saturating_sub(1);
    }

    pub(super) fn select_next(&mut self) {
        let index = self.tab.index();
        self.selected[index] = (self.selected[index] + 1).min(self.rows().len().saturating_sub(1));
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
                    self.notice = Some("Wait for the current reply.".into());
                    return;
                }
                self.runner = item["name"].as_str().map(str::to_owned);
                self.new_chat();
                self.tab = Tab::Chat;
            }
            Tab::Sessions => {
                if self.pending {
                    self.notice = Some("Wait for the current reply.".into());
                    return;
                }
                if let Some(id) = item["id"].as_str() {
                    if let Err(error) = self.open_session(id).await {
                        self.notice = Some(error.to_string());
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
                        Err(error) => self.notice = Some(error.to_string()),
                    }
                    self.tab = Tab::Chat;
                }
            }
            _ => {}
        }
    }

    pub(super) fn new_chat(&mut self) {
        self.session = None;
        self.session_label = None;
        self.entries.clear();
        self.streamed.clear();
        self.scroll = 0;
        self.unread = false;
        self.notice = None;
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
        self.load_turns(&value["turns"]);
        Ok(())
    }

    pub(super) fn load_turns(&mut self, turns: &Value) {
        let mut awaiting_output = VecDeque::new();
        for turn in turns.as_array().into_iter().flatten() {
            match turn["role"].as_str() {
                Some("tool") => {
                    let output = presentation::tool_output(turn["content"].as_str().unwrap_or(""));
                    match awaiting_output.pop_front() {
                        Some(index) => append_output(&mut self.entries[index], &output),
                        None => self.push_tool("Result", output),
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
                let (title, body) = presentation::tool_call(call);
                awaiting_output.push_back(self.entries.len());
                self.push_tool(title, body);
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
        if self.pending {
            return Err(anyhow!("wait for the current reply"));
        }
        let runner = self
            .runner
            .clone()
            .context("select a runner in the Runners tab")?;
        if self.session.is_none() {
            let created = self
                .call("sessions.create", json!({ "runner": runner }))
                .await?;
            self.session = Some(
                created["id"]
                    .as_str()
                    .context("session has no id")?
                    .to_owned(),
            );
            self.refresh_sessions().await;
        }
        let params =
            json!({ "name": runner, "prompt": input, "session_id": self.session, "stream": true });
        self.push(EntryKind::User, input);
        self.pending = true;
        self.started = Some(std::time::Instant::now());
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.cancel = Some(cancel_tx);
        self.streamed.clear();
        let base = self.base.clone();
        let timeout = self.timeout;
        let tx = self.updates.clone();
        tokio::spawn(async move {
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
            "new" => {
                if self.pending {
                    return Err(anyhow!("wait for the current reply"));
                }
                self.new_chat();
            }
            "runner" => {
                if self.pending {
                    return Err(anyhow!("wait for the current reply"));
                }
                if !self.runners.iter().any(|item| item["name"] == argument) {
                    return Err(anyhow!("runner `{argument}` is not registered"));
                }
                self.runner = Some(argument.to_owned());
                self.new_chat();
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
                    Err(error) => self.push(EntryKind::Error, error.to_string()),
                }
            }
            Update::ActionFinished { name, result } => {
                self.notice = None;
                match result.and_then(response_result) {
                    Ok(value) => self.push_tool(name, presentation::action_result(&value)),
                    Err(error) => self.push(EntryKind::Error, format!("{name}: {error}")),
                }
            }
            Update::Approval(request) => {
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
                self.notice = Some(format!("Approvals unavailable: {error}"));
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
            self.notice = Some("Approval connection closed".into());
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

    pub(super) fn cancel_run(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
            self.notice = Some(CANCELLING.into());
        }
    }

    pub(super) fn latest_expandable_tool(&self) -> Option<usize> {
        self.entries.iter().rposition(Entry::is_expandable_tool)
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
            rendered: RefCell::new(None),
        }
    }

    pub(super) fn is_expandable_tool(&self) -> bool {
        matches!(self.kind, EntryKind::Tool) && !self.body.is_empty()
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

fn append_output(entry: &mut Entry, output: &str) {
    if !entry.body.is_empty() && !output.is_empty() {
        entry.body.push_str("\n\n");
    }
    entry.body.push_str(output);
    entry.rendered.get_mut().take();
}

fn response_result(response: WsResponse) -> Result<Value> {
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        let message = response.error.as_deref().unwrap_or("unknown error");
        match response.tip.as_deref() {
            Some(tip) => Err(anyhow!("{message}\n{tip}")),
            None => Err(anyhow!("{message}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(app.entries[1].body, "Staged: Yes\n\n+line");
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
}

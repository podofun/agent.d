mod app;
mod clipboard;
mod commands;
mod control;
mod editor;
mod presentation;
mod remote;
mod search;
mod ui;

use std::io::{IsTerminal, stdout};
use std::time::Duration;

use anyhow::{Result, anyhow};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::{FutureExt, StreamExt};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use app::{App, Request, Tab};
use commands::picker_title;

struct Screen(Terminal<CrosstermBackend<std::io::Stdout>>);

impl Screen {
    fn open() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = crossterm::execute!(stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        if let Err(error) = crossterm::execute!(stdout(), EnableBracketedPaste, EnableMouseCapture)
        {
            let _ = crossterm::execute!(
                stdout(),
                DisableMouseCapture,
                DisableBracketedPaste,
                LeaveAlternateScreen
            );
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout())) {
            Ok(terminal) => Ok(Self(terminal)),
            Err(error) => {
                let _ = crossterm::execute!(
                    stdout(),
                    DisableMouseCapture,
                    DisableBracketedPaste,
                    LeaveAlternateScreen
                );
                let _ = disable_raw_mode();
                Err(error.into())
            }
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = crossterm::execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        let _ = crossterm::execute!(stdout(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

pub(crate) async fn run(
    base: &str,
    timeout: u64,
    runner: Option<String>,
    session: Option<String>,
) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(anyhow!("interactive chat needs a terminal"));
    }
    let mut app = App::new(base, timeout, runner);
    app.refresh();
    if let Some(id) = session {
        app.open(&id);
    }
    control::start(
        base.to_owned(),
        timeout,
        app.updates.clone(),
        app.decisions_rx.take().expect("control receiver"),
    );
    let mut screen = Screen::open()?;
    let size = screen.0.size()?;
    let hello = serde_json::json!({
        "version": agentd_ui::PROTOCOL_VERSION,
        "client": { "kind": "tui", "width": size.width, "height": size.height, "supports": [] },
        "session": app.session,
        "folder": std::env::current_dir().ok(),
    });
    app.script.connect(remote::start(
        base.to_owned(),
        timeout,
        hello,
        app.updates.clone(),
    ));
    let mut events = EventStream::new();
    let mut spinner = tokio::time::interval(Duration::from_millis(60));
    let mut frames = tokio::time::interval(Duration::from_millis(16));
    let mut dirty = true;
    loop {
        if dirty {
            screen.0.draw(|frame| ui::draw(frame, &app))?;
            dirty = false;
            let size = screen.0.size()?;
            let terminals = ui::script_terminals(&app, Rect::new(0, 0, size.width, size.height));
            app.script.terminal_sizes(terminals);
        }
        tokio::select! {
            _ = frames.tick(), if app.script.waiting() => {
                let size = screen.0.size()?;
                let area = Rect::new(0, 0, size.width, size.height);
                if keep_view(&mut app, area, App::play_frames) {
                    app.sync_script();
                    dirty = true;
                }
            }
            update = app.received.recv() => {
                if let Some(update) = update {
                    let size = screen.0.size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    let mut updates = vec![update];
                    while let Ok(update) = app.received.try_recv() {
                        updates.push(update);
                    }
                    apply_updates(&mut app, area, updates);
                    app.send_queued().await;
                    app.sync_script();
                    dirty = true;
                }
            }
            event = events.next() => {
                let Some(mut event) = event else { break };
                // Wheel ticks and mouse moves come in bursts: handle every
                // event already waiting, then draw once.
                let mut redraw = false;
                for _ in 0..BURST {
                    let size = screen.0.size()?;
                    let area = Rect::new(0, 0, size.width, size.height);
                    match handle_event(&mut app, event?, area).await? {
                        Handled::Quit => return Ok(()),
                        Handled::Redraw => redraw = true,
                        Handled::Ignored => {}
                    }
                    match events.next().now_or_never() {
                        Some(Some(next)) => event = next,
                        _ => break,
                    }
                }
                if redraw {
                    // Picking a runner sends what waited for one.
                    app.send_queued().await;
                    app.sync_script();
                    dirty = true;
                }
            }
            _ = spinner.tick(), if app.pending
                || app.animating()
                || app.drag.is_some()
                || app.loading(Request::Open) => {
                // A drag held above or below the conversation keeps scrolling.
                if app.drag.is_some() {
                    let size = screen.0.size()?;
                    drag_selection(&mut app, Rect::new(0, 0, size.width, size.height));
                }
                dirty = true;
            }
        }
    }
    Ok(())
}

/// The most input events handled before the screen is drawn again.
const BURST: usize = 64;

enum Handled {
    Quit,
    Redraw,
    Ignored,
}

async fn handle_event(app: &mut App, event: Event, area: Rect) -> Result<Handled> {
    app.resized(area);
    match event {
        Event::Paste(text) if app.tab == Tab::Chat && app.approval.is_none() => {
            if !paste_into_script(app, &text) {
                app.input.insert(&text);
                app.input_changed();
            }
        }
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            if handle_key(app, key, area).await? {
                return Ok(Handled::Quit);
            }
        }
        Event::Mouse(mouse) => match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if app.approval.is_none() && app.tab == Tab::Chat =>
            {
                app.selection = None;
                app.drag = None;
                if let Some(node) = ui::script_hit(app, area, mouse.column, mouse.row) {
                    click_script(app, node);
                    return Ok(Handled::Redraw);
                }
                if app.script.focus.is_some() {
                    blur_script(app);
                }
                app.press = ui::tool_at(app, area, mouse.column, mouse.row)
                    .map(|row| (row, mouse.column, mouse.row));
                if app.press.is_some() {
                    return Ok(Handled::Redraw);
                }
                if let Some(index) = ui::suggestion_at(app, area, mouse.column, mouse.row) {
                    let execute = picker_title(app.input.text()).is_some()
                        && app.suggestions()[index].execute;
                    app.complete_suggestion(index);
                    if execute && submit_draft(app).await? {
                        return Ok(Handled::Quit);
                    }
                } else if let Some(point) = ui::point_at(app, area, mouse.column, mouse.row) {
                    app.selection = Some(ui::Selection::at(point));
                } else if ui::leaf_area(app, area).is_some_and(|leaf| {
                    (leaf.x..leaf.right()).contains(&mouse.column)
                        && (leaf.y..leaf.bottom()).contains(&mouse.row)
                }) {
                    app.spin_leaf();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if app.tab == Tab::Chat => {
                // A drag that began on a tool row selects its text instead.
                if let Some((_, x, y)) = app.press.take() {
                    app.selection = ui::point_at(app, area, x, y).map(ui::Selection::at);
                }
                if app.selection.is_some() {
                    app.drag = Some((mouse.column, mouse.row));
                    drag_selection(app, area);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if app.tab == Tab::Chat => {
                app.drag = None;
                if let Some((pressed, _, _)) = app.press.take() {
                    if ui::tool_at(app, area, mouse.column, mouse.row) == Some(pressed) {
                        ui::toggle_tool(app, area, pressed);
                    }
                    return Ok(Handled::Redraw);
                }
                // A press and release on the same cell is a click, not a
                // selection: toggle the tool row under it, if any.
                if app.selection.is_some_and(|selection| selection.is_click()) {
                    app.selection = None;
                    if let Some(index) = ui::tool_at(app, area, mouse.column, mouse.row) {
                        ui::toggle_tool(app, area, index);
                    }
                }
            }
            MouseEventKind::Moved => {
                // Redraw only when what is under the mouse changes.
                let hover = (app.tab == Tab::Chat && app.approval.is_none())
                    .then(|| ui::clickable_at(app, area, mouse.column, mouse.row))
                    .flatten();
                let element = ui::script_hit(app, area, mouse.column, mouse.row);
                if hover == app.hover && element == app.script.hover {
                    return Ok(Handled::Ignored);
                }
                if element != app.script.hover
                    && let Some(node) = element
                {
                    app.script.event(Some(node), agentd_ui::Event::Hover);
                }
                app.script.hover = element;
                app.hover = hover;
            }
            MouseEventKind::ScrollUp => {
                if app.approval.is_some() {
                    app.approval_scroll = app.approval_scroll.saturating_sub(3);
                } else if app.tab == Tab::Chat {
                    scroll_up(app, area, 3);
                    app.hover = ui::clickable_at(app, area, mouse.column, mouse.row);
                } else {
                    app.move_selection(-1, false);
                }
            }
            MouseEventKind::ScrollDown => {
                if app.approval.is_some() {
                    app.approval_scroll = app.approval_scroll.saturating_add(3);
                } else if app.tab == Tab::Chat {
                    scroll_down(app, 3);
                    app.hover = ui::clickable_at(app, area, mouse.column, mouse.row);
                } else {
                    app.move_selection(1, false);
                }
            }
            _ => return Ok(Handled::Ignored),
        },
        Event::Resize(width, height) => {
            app.resized(Rect::new(0, 0, width, height));
            app.script
                .event(None, agentd_ui::Event::Resize { width, height });
        }
        _ => return Ok(Handled::Ignored),
    }
    Ok(Handled::Redraw)
}

async fn handle_key(app: &mut App, key: KeyEvent, screen: Rect) -> Result<bool> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if key.code == KeyCode::Char('c') && control {
        return Ok(handle_interrupt(app));
    }
    if app.approval.is_some() {
        handle_approval_key(app, key);
        return Ok(false);
    }
    if app.tab == Tab::Chat && handle_script_key(app, key) {
        return Ok(false);
    }
    if key.code == KeyCode::Char('p') && control {
        app.tab = if app.tab == Tab::Chat {
            Tab::All
        } else {
            Tab::Chat
        };
        return Ok(false);
    }
    if app.tab != Tab::Chat {
        handle_browser_key(app, key);
        return Ok(false);
    }
    if handle_suggestion_key(app, key) {
        return Ok(false);
    }
    if let Some(quit) = handle_chat_key(app, key, screen).await? {
        return Ok(quit);
    }
    let previous = app.input.text().to_owned();
    if let editor::Edit::Notice(notice) = app
        .input
        .handle(key, screen.width.saturating_sub(6) as usize)
    {
        app.notice = Some(notice);
    }
    if app.input.text() != previous {
        app.input_changed();
    }
    Ok(false)
}

/// Keys for the focused script element, or a shortcut a script bound.
/// Ctrl+C, the approval window and Shift+Tab never get here. Returns true
/// when the key was used.
fn handle_script_key(app: &mut App, key: KeyEvent) -> bool {
    use agentd_ui::Event;
    if key.code == KeyCode::BackTab {
        return false;
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if let Some(focus) = app.script.focus {
        let kind = app
            .script
            .tree
            .get(focus)
            .map(|n| n.kind.clone())
            .unwrap_or_default();
        if (key.code == KeyCode::Char('q') && control)
            || (key.code == KeyCode::Esc && kind != "terminal")
        {
            blur_script(app);
            return true;
        }
        match (kind.as_str(), key.code) {
            ("terminal", _) => {
                if let Some(data) = remote::key_bytes(&key) {
                    app.script.event(Some(focus), Event::Input { data });
                }
                return true;
            }
            ("input", code) => {
                let mut value = app
                    .script
                    .tree
                    .get(focus)
                    .and_then(|n| n.props.get("value"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                match code {
                    KeyCode::Char(c) if !control => value.push(c),
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    _ => {
                        if let Some(name) = remote::key_name(&key) {
                            app.script.event(Some(focus), Event::Key { key: name });
                        }
                        return true;
                    }
                }
                set_script_value(app, focus, "value", serde_json::json!(value));
                return true;
            }
            ("button" | "checkbox", KeyCode::Enter | KeyCode::Char(' ')) => {
                click_script(app, focus);
                return true;
            }
            ("list", KeyCode::Up | KeyCode::Down) => {
                let Some(node) = app.script.tree.get(focus) else {
                    return true;
                };
                let count = node.children.len() as i64;
                let selected = node
                    .props
                    .get("selected")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let step = if key.code == KeyCode::Up { -1 } else { 1 };
                let next = (selected + step).clamp(1, count.max(1));
                set_script_value(app, focus, "selected", serde_json::json!(next));
                return true;
            }
            ("list", KeyCode::Enter) => {
                let child = app.script.tree.get(focus).and_then(|n| {
                    let selected = n.props.get("selected")?.as_u64()? as usize;
                    n.children.get(selected.checked_sub(1)?).copied()
                });
                if let Some(child) = child {
                    app.script.event(Some(child), Event::Click);
                }
                return true;
            }
            _ => blur_script(app),
        }
    }
    if let Some(name) = remote::key_name(&key)
        && !agentd_ui::is_reserved_key(&name)
        && app.script.tree.bindings.contains(&name)
    {
        app.script.event(None, Event::Key { key: name });
        return true;
    }
    false
}

/// Change a value the user edits, here and for the scripts.
fn set_script_value(app: &mut App, node: agentd_ui::NodeId, key: &str, value: serde_json::Value) {
    app.script.tree.apply(&agentd_ui::Op::Set {
        node,
        key: key.into(),
        value: value.clone(),
    });
    app.script
        .event(Some(node), agentd_ui::Event::Change { value });
}

fn blur_script(app: &mut App) {
    if let Some(old) = app.script.focus.take() {
        app.script.event(Some(old), agentd_ui::Event::Blur);
    }
}

/// A click on a script element: focus what takes focus, select a list
/// item, tick a checkbox, and tell the scripts.
fn click_script(app: &mut App, node: agentd_ui::NodeId) {
    const FOCUSABLE: [&str; 5] = ["input", "terminal", "button", "checkbox", "list"];
    let tree = &app.script.tree;
    let mut focus = None;
    let mut list_pick = None;
    let mut at = Some(node);
    let mut below = None;
    while let Some(n) = at {
        let Some(el) = tree.get(n) else { break };
        if focus.is_none() && FOCUSABLE.contains(&el.kind.as_str()) {
            focus = Some(n);
        }
        if el.kind == "list"
            && list_pick.is_none()
            && let Some(child) = below
            && let Some(index) = el.children.iter().position(|c| *c == child)
        {
            list_pick = Some((n, index + 1));
        }
        below = Some(n);
        at = el.parent;
    }
    if focus != app.script.focus {
        blur_script(app);
        if let Some(f) = focus {
            app.script.focus = Some(f);
            app.script.event(Some(f), agentd_ui::Event::Focus);
        }
    }
    if let Some((list, index)) = list_pick {
        set_script_value(app, list, "selected", serde_json::json!(index));
    }
    if app
        .script
        .tree
        .get(node)
        .is_some_and(|n| n.kind == "checkbox")
    {
        let checked = app
            .script
            .tree
            .get(node)
            .and_then(|n| n.props.get("checked"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        set_script_value(app, node, "checked", serde_json::json!(!checked));
    }
    app.script.event(Some(node), agentd_ui::Event::Click);
}

/// Paste into a focused terminal or input. Returns whether it went there.
fn paste_into_script(app: &mut App, text: &str) -> bool {
    let Some(focus) = app.script.focus else {
        return false;
    };
    match app.script.tree.get(focus).map(|n| n.kind.as_str()) {
        Some("terminal") => {
            app.script
                .event(Some(focus), agentd_ui::Event::Input { data: text.into() });
            true
        }
        Some("input") => {
            let mut value = app
                .script
                .tree
                .get(focus)
                .and_then(|n| n.props.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            value.push_str(&text.replace(['\n', '\r'], " "));
            set_script_value(app, focus, "value", serde_json::json!(value));
            true
        }
        _ => false,
    }
}

/// Ctrl+C: deny, cancel, copy, clear, or quit, in that order of relevance.
/// Quitting takes a second Ctrl+C, so a missed copy never closes the chat.
fn handle_interrupt(app: &mut App) -> bool {
    if app.approval.is_some() {
        app.decide("deny");
    } else if app.pending {
        app.cancel_run();
    } else if let Some(result) = app.copy_selection().or_else(|| app.input.copy_selection()) {
        if let Err(error) = result {
            app.fail(error);
        }
    } else if !app.input.is_empty() {
        app.input.take();
        app.notice = None;
    } else if app.quit_armed() {
        return true;
    } else {
        app.quit_pressed = Some(std::time::Instant::now());
    }
    false
}

fn handle_approval_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Up => app.approval_scroll = app.approval_scroll.saturating_sub(1),
        KeyCode::PageUp => app.approval_scroll = app.approval_scroll.saturating_sub(5),
        KeyCode::Down => app.approval_scroll = app.approval_scroll.saturating_add(1),
        KeyCode::PageDown => app.approval_scroll = app.approval_scroll.saturating_add(5),
        KeyCode::Char('o') => app.decide("allow_once"),
        KeyCode::Char('f') => app.decide("allow_forever"),
        KeyCode::Char('d') | KeyCode::Esc => app.decide("deny"),
        _ => {}
    }
}

fn handle_browser_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc if !app.query.is_empty() => app.set_query(String::new()),
        KeyCode::Esc => app.tab = Tab::Chat,
        KeyCode::Backspace => {
            let mut query = app.query.clone();
            query.pop();
            app.set_query(query);
        }
        KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            let mut query = app.query.clone();
            query.push(c);
            app.set_query(query);
        }
        KeyCode::Tab | KeyCode::Right => app.tab = app.tab.next(),
        KeyCode::BackTab | KeyCode::Left => app.tab = app.tab.previous(),
        KeyCode::F(5) => app.refresh(),
        KeyCode::Up => app.move_selection(-1, true),
        KeyCode::Down => app.move_selection(1, true),
        KeyCode::PageUp => app.move_selection(-10, false),
        KeyCode::PageDown => app.move_selection(10, false),
        KeyCode::Home => app.move_selection(isize::MIN / 2, false),
        KeyCode::End => app.move_selection(isize::MAX / 2, false),
        KeyCode::Enter => app.activate_selected(),
        _ => {}
    }
}

/// Returns true when the suggestion menu consumed the key.
fn handle_suggestion_key(app: &mut App, key: KeyEvent) -> bool {
    let suggestions = app.suggestions();
    if suggestions.is_empty() || key.modifiers.contains(KeyModifiers::SHIFT) {
        return false;
    }
    let selected = app.suggestion_index.min(suggestions.len() - 1);
    match key.code {
        KeyCode::Up => app.suggestion_index = selected.saturating_sub(1),
        KeyCode::Down => app.suggestion_index = (selected + 1).min(suggestions.len() - 1),
        KeyCode::Esc => app.suggestion_dismissed = Some(app.input.text().to_owned()),
        KeyCode::Tab => app.complete_suggestion(selected),
        KeyCode::Enter => {
            let suggestion = &suggestions[selected];
            if suggestion.completion.is_none() && app.input.text().ends_with(' ') {
                return true;
            }
            if picker_title(app.input.text()).is_some() && suggestion.execute {
                app.complete_suggestion(selected);
                return false;
            }
            if suggestion.completion.is_some()
                && (suggestion.completion.as_deref() != Some(app.input.text())
                    || !suggestion.execute)
            {
                app.complete_suggestion(selected);
                return true;
            }
            return false;
        }
        _ => return false,
    }
    true
}

/// Chat level keys. `Some(quit)` when handled, `None` to fall through to the editor.
async fn handle_chat_key(app: &mut App, key: KeyEvent, screen: Rect) -> Result<Option<bool>> {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::Enter if shift => {
            app.input.insert("\n");
            app.input_changed();
        }
        KeyCode::Enter => return submit_draft(app).await.map(Some),
        KeyCode::Esc => {
            app.notice = None;
            app.selection = None;
            app.jump_to_bottom();
        }
        KeyCode::End if app.input.is_empty() => app.jump_to_bottom(),
        KeyCode::F(5) => app.refresh(),
        KeyCode::BackTab => app.cycle_approval_mode(),
        KeyCode::PageUp => scroll_up(app, screen, 8),
        KeyCode::PageDown => scroll_down(app, 8),
        KeyCode::Char('n') if control => {
            if app.pending {
                app.fail("Wait for the current reply.");
            } else {
                app.new_chat();
            }
        }
        KeyCode::Char('e') if control => {
            if let Some(row) = app.latest_tool_row() {
                ui::toggle_tool(app, screen, row);
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(false))
}

/// Extend the selection to the dragged position. Above or below the
/// conversation, it scrolls one line toward the mouse first, so holding the
/// mouse there keeps selecting.
fn drag_selection(app: &mut App, screen: Rect) {
    let Some((x, y)) = app.drag.filter(|_| app.selection.is_some()) else {
        app.drag = None;
        return;
    };
    let inner = ui::conversation_inner(ui::screen_rows(screen, app)[1]);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    // One line every 100 ms just past the edge, faster the farther out.
    let distance = if y < inner.y {
        inner.y - y
    } else {
        (y + 1).saturating_sub(inner.bottom())
    };
    let interval = std::time::Duration::from_millis(100 / u64::from(distance.clamp(1, 4)));
    let due = app
        .drag_scrolled
        .is_none_or(|scrolled| scrolled.elapsed() >= interval);
    if distance > 0 && due {
        if y < inner.y {
            scroll_up(app, screen, 1);
        } else {
            app.scroll = app.scroll.saturating_sub(1);
        }
        app.drag_scrolled = Some(std::time::Instant::now());
    }
    let x = x.clamp(inner.x, inner.right() - 1);
    let mut row = y.clamp(inner.y, inner.bottom() - 1);
    // Below the last line, select to the end of it.
    let point = loop {
        if let Some(point) = ui::point_at(app, screen, x, row) {
            break Some(point);
        }
        if row == inner.y {
            break None;
        }
        row -= 1;
    };
    if let (Some(selection), Some(point)) = (app.selection.as_mut(), point) {
        selection.head = point;
    }
}

/// Apply streamed updates. At the bottom, the view follows new lines. Scrolled
/// up, it stays on the lines being read while the reply grows below them.
fn apply_updates(app: &mut App, screen: Rect, updates: Vec<app::Update>) {
    keep_view(app, screen, |app| {
        for update in updates {
            app.apply(update);
        }
    });
}

/// Make a change that may add or drop lines, keeping a scrolled up view on
/// the same lines.
fn keep_view<T>(app: &mut App, screen: Rect, change: impl FnOnce(&mut App) -> T) -> T {
    let held = app.scroll > 0;
    let before = if held { ui::max_scroll(app, screen) } else { 0 };
    let result = change(app);
    if held && app.scroll > 0 {
        let after = ui::max_scroll(app, screen);
        app.scroll = (app.scroll + after).saturating_sub(before);
    }
    result
}

/// Scroll up, stopping at the first line. Without the stop, extra scrolling
/// past the top had to be undone before scrolling down moved anything.
fn scroll_up(app: &mut App, screen: Rect, lines: usize) {
    app.scroll = app
        .scroll
        .saturating_add(lines)
        .min(ui::max_scroll(app, screen));
}

fn scroll_down(app: &mut App, lines: usize) {
    app.scroll = app.scroll.saturating_sub(lines);
    if app.scroll == 0 {
        app.jump_to_bottom();
    }
}

async fn submit_draft(app: &mut App) -> Result<bool> {
    let input = app.input.take();
    app.input_changed();
    if matches!(input.trim(), "/quit" | "/exit") {
        return Ok(true);
    }
    if let Err(error) = app.submit(&input).await {
        app.fail(error.to_string());
        // A message that failed stays in the conversation, queued; only a
        // command comes back to the draft.
        if input.trim_start().starts_with('/') {
            app.input.set(input);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use app::{EntryKind, ToolRow};

    fn render(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// Feed ops to the chat as the daemon would, shown at once.
    fn scripted(app: &mut App, reset: bool, ops: Vec<agentd_ui::Op>) {
        if app.script.chat.is_none() {
            app.script.ready(agentd_ui::Ready {
                version: 1,
                chat: 1,
                clock: 0,
            });
        }
        app.apply(app::Update::Ui(remote::Remote::Frame(agentd_ui::Frame {
            seq: 0,
            at: 0,
            reset,
            cancel_ahead: false,
            ops,
        })));
    }

    fn text_element(node: u64, text: &str) -> Vec<agentd_ui::Op> {
        let mut props = agentd_ui::Props::new();
        props.insert("text".into(), serde_json::json!(text));
        vec![
            agentd_ui::Op::Create {
                node,
                kind: "text".into(),
                props,
                style: Default::default(),
            },
            agentd_ui::Op::Insert {
                parent: agentd_ui::parts::MAIN,
                node,
                index: 1,
            },
        ]
    }

    fn talking() -> App {
        let mut app = App::new("http://127.0.0.1:7777", 1000, Some("coder".into()));
        app.connected = true;
        app.push(EntryKind::User, "hello");
        app.push(EntryKind::Agent, "Hi! **What** shall we build?");
        app.push_tool("files.read", "fn main() {}");
        app
    }

    #[test]
    fn chat_looks_the_same_with_only_the_default_view() {
        let plain = talking();
        let mut default = talking();
        scripted(
            &mut default,
            true,
            agentd_ui::Tree::default_chat().snapshot(),
        );
        assert!(!default.script.custom());
        assert_eq!(render(&plain, 90, 24), render(&default, 90, 24));

        // Bindings alone change nothing on screen.
        scripted(
            &mut default,
            false,
            vec![agentd_ui::Op::Bindings {
                keys: vec!["ctrl+g".into()],
            }],
        );
        assert_eq!(render(&plain, 90, 24), render(&default, 90, 24));

        let mut panel = talking();
        scripted(&mut panel, true, agentd_ui::Tree::default_chat().snapshot());
        scripted(&mut panel, false, text_element(100, "todo panel"));
        assert!(panel.script.custom());
        let shown: String = render(&panel, 90, 24)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            shown.contains("todo panel") && shown.contains("What"),
            "{shown}"
        );
    }

    #[test]
    fn script_text_cannot_reach_the_terminal_as_escapes() {
        let mut app = talking();
        scripted(&mut app, true, agentd_ui::Tree::default_chat().snapshot());
        scripted(
            &mut app,
            false,
            text_element(100, "a\x1b[2Jb\x07c\u{9b}31md"),
        );
        let buffer = render(&app, 90, 24);
        let shown: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(!shown.chars().any(|c| c.is_control()), "{shown:?}");
        assert!(shown.contains('a') && shown.contains("[2Jb"));
    }

    #[tokio::test]
    async fn chat_keeps_ctrl_c_and_the_approval_keys_whatever_scripts_bind() {
        let (out, mut sent) = tokio::sync::mpsc::unbounded_channel();
        let mut app = talking();
        app.script.connect(out);
        scripted(&mut app, true, agentd_ui::Tree::default_chat().snapshot());
        let mut ops = text_element(100, "pane");
        ops[0] = agentd_ui::Op::Create {
            node: 100,
            kind: "terminal".into(),
            props: Default::default(),
            style: Default::default(),
        };
        ops.push(agentd_ui::Op::Bindings {
            keys: vec!["ctrl+c".into(), "o".into(), "backtab".into()],
        });
        scripted(&mut app, false, ops);
        app.script.focus = Some(100);
        let screen = Rect::new(0, 0, 90, 24);
        while sent.try_recv().is_ok() {}

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key(&mut app, ctrl_c, screen).await.unwrap();
        assert!(app.quit_armed(), "Ctrl+C reached chat");
        assert!(
            sent.try_recv().is_err(),
            "Ctrl+C was not sent to the script"
        );

        let request: agentd_types::ApprovalRequest = serde_json::from_value(serde_json::json!({
            "id": 7, "kind": "missing_grant", "action": "shell.run", "tool": null,
            "requires": [], "missing": [], "reason": "", "caller": {}
        }))
        .unwrap();
        app.apply(app::Update::Approval(Box::new(request)));
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('o')), screen)
            .await
            .unwrap();
        assert!(
            app.approval.is_none(),
            "the approval key answered the approval"
        );
        assert!(sent.try_recv().is_err());

        // The approval window draws over whatever the script shows.
        let request: agentd_types::ApprovalRequest = serde_json::from_value(serde_json::json!({
            "id": 8, "kind": "missing_grant", "action": "shell.run", "tool": null,
            "requires": [], "missing": [], "reason": "", "caller": {}
        }))
        .unwrap();
        app.apply(app::Update::Approval(Box::new(request)));
        scripted(
            &mut app,
            false,
            vec![agentd_ui::Op::Set {
                node: agentd_ui::parts::CONVERSATION,
                key: "visible".into(),
                value: serde_json::json!(false),
            }],
        );
        let shown: String = render(&app, 90, 24)
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(shown.contains("shell.run"), "{shown}");
    }

    #[tokio::test]
    async fn typing_in_the_browser_filters_and_esc_clears_before_closing() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.actions = vec![serde_json::json!("git.status"), serde_json::json!("notes")];
        app.tab = Tab::All;
        let screen = Rect::new(0, 0, 80, 24);
        for c in ['g', 'i'] {
            handle_key(&mut app, KeyEvent::from(KeyCode::Char(c)), screen)
                .await
                .unwrap();
        }
        assert_eq!(app.query, "gi");
        assert_eq!(app.rows().len(), 1);
        handle_key(&mut app, KeyEvent::from(KeyCode::Backspace), screen)
            .await
            .unwrap();
        assert_eq!(app.query, "g");
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc), screen)
            .await
            .unwrap();
        assert_eq!(app.query, "");
        assert_eq!(app.tab, Tab::All);
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc), screen)
            .await
            .unwrap();
        assert_eq!(app.tab, Tab::Chat);
    }

    #[tokio::test]
    async fn a_new_query_resets_the_selection() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.actions = vec![serde_json::json!("a"), serde_json::json!("b")];
        app.tab = Tab::All;
        app.move_selection(1, true);
        assert_eq!(app.selected_index(), 1);
        let screen = Rect::new(0, 0, 80, 24);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('b')), screen)
            .await
            .unwrap();
        assert_eq!(app.selected_index(), 0);
    }

    #[tokio::test]
    async fn browser_sections_cycle_without_closing() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::All;
        let screen = Rect::new(0, 0, 80, 24);
        let mut seen = Vec::new();
        for _ in 0..Tab::ALL.len() - 1 {
            handle_key(&mut app, KeyEvent::from(KeyCode::Right), screen)
                .await
                .unwrap();
            seen.push(app.tab);
        }
        assert_eq!(
            seen,
            [
                Tab::Sessions,
                Tab::Runners,
                Tab::Skills,
                Tab::Actions,
                Tab::Services,
                Tab::All
            ]
        );
        handle_key(&mut app, KeyEvent::from(KeyCode::Left), screen)
            .await
            .unwrap();
        assert_eq!(app.tab, Tab::Services);
        handle_key(&mut app, KeyEvent::from(KeyCode::BackTab), screen)
            .await
            .unwrap();
        assert_eq!(app.tab, Tab::Actions);
    }

    #[tokio::test]
    async fn arrow_keys_wrap_and_page_keys_stop_at_the_ends() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.actions = (0..15)
            .map(|i| serde_json::json!(format!("a{i}")))
            .collect();
        app.tab = Tab::Actions;
        let screen = Rect::new(0, 0, 80, 24);
        let press = |code| KeyEvent::from(code);
        handle_key(&mut app, press(KeyCode::Up), screen)
            .await
            .unwrap();
        assert_eq!(app.selected_index(), 14);
        handle_key(&mut app, press(KeyCode::Down), screen)
            .await
            .unwrap();
        assert_eq!(app.selected_index(), 0);
        handle_key(&mut app, press(KeyCode::PageDown), screen)
            .await
            .unwrap();
        handle_key(&mut app, press(KeyCode::PageDown), screen)
            .await
            .unwrap();
        assert_eq!(app.selected_index(), 14);
        handle_key(&mut app, press(KeyCode::Home), screen)
            .await
            .unwrap();
        assert_eq!(app.selected_index(), 0);
        handle_key(&mut app, press(KeyCode::End), screen)
            .await
            .unwrap();
        assert_eq!(app.selected_index(), 14);
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn chat_with_tool() -> (App, Rect, u16, u16) {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "hello world");
        app.push_tool("git.status", "one\ntwo");
        let screen = Rect::new(0, 0, 60, 24);
        let inner = ui::conversation_inner(ui::screen_rows(screen, &app)[1]);
        (app, screen, inner.x, inner.y)
    }

    #[tokio::test]
    async fn dragging_selects_text_and_ctrl_c_keeps_the_draft() {
        let (mut app, screen, x, y) = chat_with_tool();
        app.input.set("draft");
        handle_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), x + 3, y),
            screen,
        )
        .await
        .unwrap();
        handle_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), x + 7, y),
            screen,
        )
        .await
        .unwrap();
        handle_event(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), x + 7, y),
            screen,
        )
        .await
        .unwrap();
        let selection = app.selection.expect("selection");
        assert_eq!(selection.anchor, ui::Point { line: 0, col: 3 });
        assert_eq!(selection.head, ui::Point { line: 0, col: 7 });
        assert_eq!(app.selected_text().as_deref(), Some("hello"));

        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!handle_key(&mut app, key, screen).await.unwrap());
        assert_eq!(app.input.text(), "draft", "copy never clears the draft");
        assert!(app.selection.is_none(), "copy releases the selection");
    }

    #[tokio::test]
    async fn errors_show_in_the_conversation() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        app.input.set("/approval maybe");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter, screen).await.unwrap();
        let last = app.entries.last().unwrap();
        assert!(matches!(last.kind, EntryKind::Error));
        assert!(last.text.contains("usage: /approval"));
        assert!(app.notice.is_none());
        assert_eq!(app.input.text(), "/approval maybe", "the draft comes back");
    }

    #[tokio::test]
    async fn dragging_above_the_conversation_scrolls_and_keeps_selecting() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        for turn in 0..30 {
            app.push(EntryKind::User, format!("message {turn}"));
        }
        let inner = ui::conversation_inner(ui::screen_rows(screen, &app)[1]);
        let (x, y) = (inner.x + 2, inner.bottom() - 1);
        handle_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            screen,
        )
        .await
        .unwrap();
        let anchor = app.selection.unwrap().anchor;
        handle_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), x, 0),
            screen,
        )
        .await
        .unwrap();
        assert_eq!(app.scroll, 1, "the drag scrolled up one line");
        // Right away, it waits: scrolling is paced so it can be stopped.
        drag_selection(&mut app, screen);
        assert_eq!(app.scroll, 1);
        // Holding the mouse there keeps scrolling, as the timer would.
        for _ in 0..4 {
            app.drag_scrolled = None;
            drag_selection(&mut app, screen);
        }
        assert_eq!(app.scroll, 5);
        let selection = app.selection.unwrap();
        assert_eq!(selection.anchor, anchor);
        assert_eq!(
            selection.head.line,
            anchor.line - inner.height as usize + 1 - 5
        );

        app.drag_scrolled = None;
        handle_event(
            &mut app,
            mouse(MouseEventKind::Drag(MouseButton::Left), x, 23),
            screen,
        )
        .await
        .unwrap();
        assert_eq!(app.scroll, 4, "below the conversation it scrolls down");
        handle_event(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), x, 23),
            screen,
        )
        .await
        .unwrap();
        assert!(app.drag.is_none());
    }

    #[test]
    fn streaming_keeps_a_scrolled_up_view_still_and_follows_at_the_bottom() {
        use app::Update;
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        for turn in 0..30 {
            app.push(EntryKind::User, format!("message {turn}"));
        }
        app.pending = true;
        let chunk =
            || Update::Delta(serde_json::json!({ "type": "text_delta", "text": "line\n\n" }));
        app.scroll = 5;
        let top = |app: &App| ui::max_scroll(app, screen) - app.scroll;
        let reading = top(&app);
        apply_updates(&mut app, screen, vec![chunk(), chunk(), chunk()]);
        assert!(app.scroll > 5, "the view moved up with the new lines");
        assert_eq!(top(&app), reading, "the same lines stay on screen");

        app.scroll = 0;
        apply_updates(&mut app, screen, vec![chunk()]);
        assert_eq!(app.scroll, 0, "at the bottom it follows the reply");
    }

    #[tokio::test]
    async fn scrolling_up_stops_at_the_top() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        for turn in 0..30 {
            app.push(EntryKind::User, format!("message {turn}"));
        }
        let top = ui::max_scroll(&app, screen);
        assert!(top > 0);
        for _ in 0..100 {
            scroll_up(&mut app, screen, 3);
        }
        assert_eq!(app.scroll, top);
        scroll_down(&mut app, 3);
        assert_eq!(app.scroll, top - 3, "scrolling down moves at once");
    }

    #[tokio::test]
    async fn opening_a_chat_shows_it_loading_and_the_newest_open_wins() {
        let mut app = App::new("http://127.0.0.1:9", 1000, None);
        app.sessions = vec![serde_json::json!({ "id": "first", "label": "First" })];
        app.tab = Tab::Sessions;
        let screen = Rect::new(0, 0, 80, 24);
        handle_key(&mut app, KeyEvent::from(KeyCode::Enter), screen)
            .await
            .unwrap();
        assert_eq!(app.tab, Tab::Chat, "the browser closes at once");
        let text: String = render(&app, 80, 24)
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Loading conversation…"), "{text}");

        let first = app.requests[&Request::Open].0;
        app.submit("/open second").await.unwrap();
        let second = app.requests[&Request::Open].0;
        let session = |id: &str| {
            Ok(app::Loaded::Session(serde_json::json!({
                "id": id,
                "turns": [{ "role": "user", "content": format!("hi from {id}") }],
            })))
        };
        app.apply(app::Update::Loaded {
            request: Request::Open,
            id: first,
            result: session("first"),
        });
        assert!(app.session.is_none(), "a replaced open is dropped");
        app.apply(app::Update::Loaded {
            request: Request::Open,
            id: second,
            result: session("second"),
        });
        assert_eq!(app.session.as_deref(), Some("second"));
        assert!(!app.loading(Request::Open));
        assert_eq!(app.entries[0].text, "hi from second");

        app.open("third");
        let third = app.requests[&Request::Open].0;
        app.apply(app::Update::Loaded {
            request: Request::Open,
            id: third,
            result: Err(anyhow!("session not found")),
        });
        assert!(!app.loading(Request::Open));
        assert_eq!(
            app.entries.last().unwrap().text,
            "Could not open the chat: session not found"
        );
    }

    #[tokio::test]
    async fn scrolling_a_long_chat_draws_without_building_it_again() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let reply = "Some **text**.\n\n```rust\nfn main() {\n    run();\n}\n```\n";
        for turn in 0..200 {
            app.push(EntryKind::User, format!("question {turn}"));
            app.push_tool("git.diff", "line\n".repeat(20));
            app.push_markdown(EntryKind::Agent, reply);
        }
        let screen = Rect::new(0, 0, 100, 40);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        let builds = || ui::BUILDS.with(std::cell::Cell::get);
        let before = builds();
        for _ in 0..10 {
            handle_event(&mut app, mouse(MouseEventKind::ScrollUp, 10, 10), screen)
                .await
                .unwrap();
        }
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        assert_eq!(app.scroll, 30);
        assert_eq!(builds(), before, "scrolling reuses the built conversation");

        // A change to an entry builds it again, and keeps the view in place.
        let top = ui::max_scroll(&app, screen) - app.scroll;
        let last = app.entries.len() - 2;
        keep_view(&mut app, screen, |app| app.entries[last].set_expanded(true));
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        assert_eq!(builds(), before + 1);
        assert_eq!(ui::max_scroll(&app, screen) - app.scroll, top);
    }

    #[tokio::test]
    async fn quitting_takes_two_ctrl_c() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        app.input.set("draft");
        assert!(!handle_key(&mut app, key, screen).await.unwrap());
        assert!(app.input.is_empty(), "the first clears the draft");
        assert!(!handle_key(&mut app, key, screen).await.unwrap());
        assert!(app.quit_armed());
        assert!(handle_key(&mut app, key, screen).await.unwrap());
    }

    #[tokio::test]
    async fn a_click_without_dragging_still_toggles_a_tool_row() {
        let (mut app, screen, x, y) = chat_with_tool();
        let header = y + 2;
        handle_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), x + 4, header),
            screen,
        )
        .await
        .unwrap();
        assert!(
            !app.entries[1].expanded,
            "nothing happens until the button is released"
        );
        assert!(
            app.selection.is_none(),
            "pressing on a tool row does not start a selection"
        );
        handle_event(
            &mut app,
            mouse(MouseEventKind::Up(MouseButton::Left), x + 4, header),
            screen,
        )
        .await
        .unwrap();
        assert!(app.entries[1].expanded);
        assert!(app.selection.is_none());
    }

    #[tokio::test]
    async fn hovering_a_tool_row_highlights_it_until_the_mouse_leaves() {
        let (mut app, screen, x, y) = chat_with_tool();
        let header = y + 2;
        let moved = |column, row| mouse(MouseEventKind::Moved, column, row);
        let handled = handle_event(&mut app, moved(x + 4, header), screen)
            .await
            .unwrap();
        assert!(matches!(handled, Handled::Redraw));
        assert!(
            matches!(app.hover, Some((_, ToolRow::Entry(1)))),
            "{:?}",
            app.hover
        );

        let backend = ratatui::backend::TestBackend::new(screen.width, screen.height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        let highlighted = |terminal: &ratatui::Terminal<ratatui::backend::TestBackend>| {
            (0..screen.width).any(|column| {
                let cell = &terminal.backend().buffer()[(column, header)];
                assert!(
                    !cell.modifier.contains(ratatui::style::Modifier::UNDERLINED),
                    "hover never underlines"
                );
                cell.symbol() == "g" && cell.fg != ui::MUTED
            })
        };
        assert!(highlighted(&terminal));

        let handled = handle_event(&mut app, moved(x + 5, header), screen)
            .await
            .unwrap();
        assert!(
            matches!(handled, Handled::Ignored),
            "moving within the same row does not redraw"
        );
        handle_event(&mut app, moved(x + 4, y), screen)
            .await
            .unwrap();
        assert_eq!(app.hover, None);
        terminal.draw(|frame| ui::draw(frame, &app)).unwrap();
        assert!(!highlighted(&terminal));
    }

    #[tokio::test]
    async fn esc_resize_and_a_new_chat_drop_the_selection() {
        let (mut app, screen, x, y) = chat_with_tool();
        let select = |app: &mut App| {
            app.selection = Some(ui::Selection {
                anchor: ui::Point { line: 0, col: 3 },
                head: ui::Point { line: 0, col: 7 },
            });
        };
        select(&mut app);
        handle_key(&mut app, KeyEvent::from(KeyCode::Esc), screen)
            .await
            .unwrap();
        assert!(app.selection.is_none());
        select(&mut app);
        handle_event(&mut app, Event::Resize(80, 24), screen)
            .await
            .unwrap();
        assert!(app.selection.is_none());
        select(&mut app);
        app.new_chat();
        assert!(app.selection.is_none());
        select(&mut app);
        handle_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), x, y + 20),
            screen,
        )
        .await
        .unwrap();
        assert!(
            app.selection.is_none(),
            "a click outside the conversation clears it"
        );
    }

    #[tokio::test]
    async fn clicking_the_leaf_turns_it_once() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.connected = true;
        let screen = Rect::new(0, 0, 100, 40);
        let leaf = ui::leaf_area(&app, screen).expect("leaf shown");
        let (x, y) = (leaf.x + leaf.width / 2, leaf.y + leaf.height / 2);
        assert!(!app.animating());
        handle_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), 0, y),
            screen,
        )
        .await
        .unwrap();
        assert!(!app.animating(), "a click beside the leaf does nothing");
        handle_event(
            &mut app,
            mouse(MouseEventKind::Down(MouseButton::Left), x, y),
            screen,
        )
        .await
        .unwrap();
        assert!(app.animating());
        app.leaf_spin = Some(std::time::Instant::now() - ui::LEAF_SPIN);
        assert!(!app.animating(), "one turn, then it rests");

        app.push(EntryKind::User, "hi");
        assert!(
            ui::leaf_area(&app, screen).is_none(),
            "no leaf once a chat starts"
        );
    }

    #[tokio::test]
    async fn ctrl_e_toggles_latest_tool_without_editing_draft() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("git.status", "On branch main");
        app.input.set("draft");
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL);
        let screen = Rect::new(0, 0, 80, 20);
        assert!(!handle_key(&mut app, key, screen).await.unwrap());
        assert!(app.entries[0].expanded);
        assert_eq!(app.input.text(), "draft");
        handle_key(&mut app, key, screen).await.unwrap();
        assert!(!app.entries[0].expanded);
    }

    #[tokio::test]
    async fn slash_menu_completes_and_dismisses_without_sending() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        app.input.set("/ru");
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        handle_key(&mut app, down, screen).await.unwrap();
        assert_eq!(app.suggestion_index, 0);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter, screen).await.unwrap();
        assert_eq!(app.input.text(), "/runner ");
        assert!(app.entries.is_empty());
        app.input.set("/");
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, escape, screen).await.unwrap();
        assert!(app.suggestions().is_empty());
        let character = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE);
        handle_key(&mut app, character, screen).await.unwrap();
        assert_eq!(app.suggestions()[0].label, "/new");
    }

    #[tokio::test]
    async fn dismissed_menu_stays_dismissed_across_navigation_keys() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        app.input.set("/");
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        handle_key(&mut app, escape, screen).await.unwrap();
        handle_key(&mut app, escape, screen).await.unwrap();
        assert!(app.suggestions().is_empty());
        let page_up = KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE);
        handle_key(&mut app, page_up, screen).await.unwrap();
        assert!(app.suggestions().is_empty());
    }

    #[tokio::test]
    async fn plain_runner_opens_searchable_picker_and_enter_selects() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.runners = vec![serde_json::json!({ "name": "helper", "model": "mock/test" })];
        app.input.set("/runner");
        let screen = Rect::new(0, 0, 80, 24);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter, screen).await.unwrap();
        assert_eq!(app.input.text(), "/runner ");
        assert_eq!(app.suggestions()[0].label, "helper");
        handle_key(&mut app, enter, screen).await.unwrap();
        assert_eq!(app.runner.as_deref(), Some("helper"));
        assert!(app.input.is_empty());
    }

    #[tokio::test]
    async fn plain_call_opens_actions_and_prepares_command_without_running() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.actions = vec![serde_json::json!("git.status")];
        app.input.set("/call");
        let screen = Rect::new(0, 0, 80, 24);
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, enter, screen).await.unwrap();
        assert_eq!(app.input.text(), "/call ");
        assert_eq!(app.suggestions()[0].label, "git.status");
        handle_key(&mut app, enter, screen).await.unwrap();
        assert_eq!(app.input.text(), "/call git.status ");
        assert!(app.entries.is_empty());
        assert!(!app.pending);
    }

    #[tokio::test]
    async fn shift_arrows_and_alt_delete_edit_selected_text() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let screen = Rect::new(0, 0, 80, 24);
        app.input.set("hello world");
        app.input.document_start();
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT),
            screen,
        )
        .await
        .unwrap();
        assert_eq!(app.input.text(), "world");
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT),
            screen,
        )
        .await
        .unwrap();
        assert_eq!(app.input.selected_text(), Some("w"));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT),
            screen,
        )
        .await
        .unwrap();
        assert_eq!(app.input.text(), "Xorld");
    }
}

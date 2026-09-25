mod app;
mod commands;
mod control;
mod editor;
mod presentation;
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
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use app::{App, Tab};
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
    app.refresh().await;
    if let Some(id) = session
        && let Err(error) = app.open_session(&id).await
    {
        app.notice = Some(format!("Could not open session: {error:#}"));
    }
    control::start(
        base.to_owned(),
        timeout,
        app.updates.clone(),
        app.decisions_rx.take().expect("control receiver"),
    );
    let mut screen = Screen::open()?;
    let mut events = EventStream::new();
    let mut spinner = tokio::time::interval(Duration::from_millis(100));
    let mut dirty = true;
    loop {
        if dirty {
            screen.0.draw(|frame| ui::draw(frame, &app))?;
            dirty = false;
        }
        tokio::select! {
            update = app.received.recv() => {
                if let Some(update) = update {
                    app.apply(update);
                    while let Ok(update) = app.received.try_recv() {
                        app.apply(update);
                    }
                    dirty = true;
                }
            }
            event = events.next() => {
                let Some(event) = event else { break };
                let size = screen.0.size()?;
                let area = Rect::new(0, 0, size.width, size.height);
                match handle_event(&mut app, event?, area).await? {
                    Handled::Quit => break,
                    Handled::Redraw => dirty = true,
                    Handled::Ignored => {}
                }
            }
            _ = spinner.tick(), if app.pending => dirty = true,
        }
    }
    Ok(())
}

enum Handled {
    Quit,
    Redraw,
    Ignored,
}

async fn handle_event(app: &mut App, event: Event, area: Rect) -> Result<Handled> {
    match event {
        Event::Paste(text) if app.tab == Tab::Chat && app.approval.is_none() => {
            app.input.insert(&text);
            app.input_changed();
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
                if let Some(index) = ui::suggestion_at(app, area, mouse.column, mouse.row) {
                    let execute = picker_title(app.input.text()).is_some()
                        && app.suggestions()[index].execute;
                    app.complete_suggestion(index);
                    if execute && submit_draft(app).await? {
                        return Ok(Handled::Quit);
                    }
                } else if let Some(index) = ui::tool_at(app, area, mouse.column, mouse.row) {
                    ui::toggle_tool(app, area, index);
                }
            }
            MouseEventKind::ScrollUp => {
                if app.approval.is_some() {
                    app.approval_scroll = app.approval_scroll.saturating_sub(3);
                } else if app.tab == Tab::Chat {
                    app.scroll = app.scroll.saturating_add(3);
                } else {
                    app.select_previous();
                }
            }
            MouseEventKind::ScrollDown => {
                if app.approval.is_some() {
                    app.approval_scroll = app.approval_scroll.saturating_add(3);
                } else if app.tab == Tab::Chat {
                    scroll_down(app, 3);
                } else {
                    app.select_next();
                }
            }
            _ => return Ok(Handled::Ignored),
        },
        Event::Resize(_, _) => {}
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
    if key.code == KeyCode::Char('p') && control {
        app.tab = if app.tab == Tab::Chat {
            Tab::All
        } else {
            Tab::Chat
        };
        return Ok(false);
    }
    if app.tab != Tab::Chat {
        handle_browser_key(app, key).await;
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

/// Ctrl+C: deny, cancel, copy, clear, or quit, in that order of relevance.
fn handle_interrupt(app: &mut App) -> bool {
    if app.approval.is_some() {
        app.decide("deny");
    } else if app.pending {
        app.cancel_run();
    } else if let Some(result) = app.input.copy_selection() {
        if let Err(notice) = result {
            app.notice = Some(notice);
        }
    } else if !app.input.is_empty() {
        app.input.take();
        app.notice = None;
    } else {
        return true;
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

async fn handle_browser_key(app: &mut App, key: KeyEvent) {
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
        KeyCode::Tab => app.tab = app.tab.next(),
        KeyCode::BackTab => app.tab = app.tab.previous(),
        KeyCode::F(5) => app.refresh().await,
        KeyCode::Up => app.select_previous(),
        KeyCode::Down => app.select_next(),
        KeyCode::Enter => app.activate_selected().await,
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
            app.jump_to_bottom();
        }
        KeyCode::End if app.input.is_empty() => app.jump_to_bottom(),
        KeyCode::F(5) => app.refresh().await,
        KeyCode::PageUp => app.scroll = app.scroll.saturating_add(8),
        KeyCode::PageDown => scroll_down(app, 8),
        KeyCode::Char('n') if control => {
            if app.pending {
                app.notice = Some("Wait for the current reply.".into());
            } else {
                app.new_chat();
            }
        }
        KeyCode::Char('e') if control => {
            if let Some(index) = app.latest_expandable_tool() {
                ui::toggle_tool(app, screen, index);
            }
        }
        _ => return Ok(None),
    }
    Ok(Some(false))
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
        app.notice = Some(error.to_string());
        app.input.set(input);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        app.select_next();
        assert_eq!(app.selected_index(), 1);
        let screen = Rect::new(0, 0, 80, 24);
        handle_key(&mut app, KeyEvent::from(KeyCode::Char('b')), screen)
            .await
            .unwrap();
        assert_eq!(app.selected_index(), 0);
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

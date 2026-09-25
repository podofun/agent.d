use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::{App, Tab};

mod approval;
mod browser;
mod conversation;
mod markdown;

pub(super) use conversation::{suggestion_at, toggle_tool, tool_at};

pub(super) const PRIMARY: Color = Color::Rgb(177, 124, 245);
pub(super) const SECONDARY: Color = Color::Rgb(122, 180, 234);
pub(super) const MUTED: Color = Color::DarkGray;
pub(super) const SUCCESS: Color = Color::Green;
pub(super) const WARNING: Color = Color::Yellow;
pub(super) const ERROR: Color = Color::Red;
pub(super) const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub(super) fn palette() -> markdown::Palette {
    markdown::Palette {
        primary: PRIMARY,
        secondary: SECONDARY,
        muted: MUTED,
    }
}

pub(super) fn spinner_frame(app: &App) -> &'static str {
    let tick = app
        .started_at()
        .map_or(0, |started| started.elapsed().as_millis() / 120);
    SPINNER[(tick % SPINNER.len() as u128) as usize]
}

pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let rows = screen_rows(area, app);
    draw_header(frame, app, rows[0]);
    conversation::draw_conversation(frame, app, rows[1]);
    conversation::draw_suggestions(frame, app, rows[2]);
    draw_composer(frame, app, rows[3]);
    draw_status(frame, app, rows[4]);
    if app.tab != Tab::Chat {
        browser::draw_browser(frame, app);
    }
    if let Some(request) = &app.approval {
        approval::draw_approval(frame, request, app.approval_scroll);
    }
}

pub(super) fn screen_rows(area: Rect, app: &App) -> [Rect; 5] {
    let composer_lines = app
        .input
        .layout(area.width.saturating_sub(6).max(1) as usize)
        .lines
        .len();
    let composer_height = (composer_lines.min(6) as u16 + 2).max(3);
    let suggestions = if app.tab == Tab::Chat && app.approval.is_none() {
        app.suggestions().len().min(6) as u16
    } else {
        0
    };
    let available = area.height.saturating_sub(2 + 4 + composer_height + 1);
    let menu_height = if suggestions > 0 {
        (suggestions + 2).min(available)
    } else {
        0
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(menu_height),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);
    [rows[0], rows[1], rows[2], rows[3], rows[4]]
}

fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let runner = app.runner.as_deref().unwrap_or("no runner");
    let session = app
        .session_label
        .as_deref()
        .or_else(|| app.session.as_deref().map(|id| &id[..id.len().min(8)]))
        .unwrap_or("new chat");
    let line = Line::from(vec![
        Span::styled(
            format!("  {runner}"),
            Style::default().fg(SECONDARY).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  ·  {session}"), Style::default().fg(MUTED)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
    if !app.connected {
        let offline = Line::styled("offline  ", Style::default().fg(ERROR));
        let width = offline.width() as u16;
        if width < area.width {
            frame.render_widget(
                Paragraph::new(offline),
                Rect::new(area.right() - width, area.y, width, 1),
            );
        }
    }
    frame.render_widget(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(MUTED)),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
}

fn draw_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.height < 17 || area.width < 42 {
        frame.render_widget(
            Paragraph::new("What would you like to do?").style(Style::default().fg(MUTED)),
            Rect::new(
                area.x.saturating_add(2),
                area.y,
                area.width.saturating_sub(2),
                area.height,
            ),
        );
        return;
    }
    let top = area.y + area.height.saturating_sub(17) / 2;
    let logo_x = area.x + area.width.saturating_sub(24) / 2;
    for (index, line) in include_str!("../logo.txt").lines().enumerate() {
        frame.render_widget(
            Paragraph::new(line).style(Style::default().fg(PRIMARY)),
            Rect::new(logo_x, top + index as u16, 24.min(area.width), 1),
        );
    }
    let title = "agent.d";
    frame.render_widget(
        Paragraph::new(title).style(Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)),
        Rect::new(
            area.x + area.width.saturating_sub(title.len() as u16) / 2,
            top + 10,
            title.len() as u16,
            1,
        ),
    );
    let hints: &[&str] = if app.connected {
        &[
            "Type a message to start",
            "Ctrl+P picks a runner or resumes a chat",
            "/ shows commands",
        ]
    } else {
        &["Cannot reach daemon. Check the connection and press F5."]
    };
    for (index, hint) in hints.iter().enumerate() {
        frame.render_widget(
            Paragraph::new(*hint)
                .style(Style::default().fg(MUTED))
                .alignment(Alignment::Center),
            Rect::new(area.x, top + 12 + index as u16, area.width, 1),
        );
    }
}

fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let line = if let Some(notice) = &app.notice {
        Line::styled(format!("  {notice}"), Style::default().fg(ERROR))
    } else if app.pending {
        let elapsed = app
            .started_at()
            .map_or(0, |started| started.elapsed().as_secs());
        Line::styled(
            format!("  {} working  {elapsed}s", spinner_frame(app)),
            Style::default().fg(WARNING),
        )
    } else if app.has_unread() {
        Line::styled(
            "  ↓ new messages  ·  End jumps to the bottom",
            Style::default().fg(SECONDARY),
        )
    } else {
        Line::styled(
            "  Enter send  ·  Shift+Enter newline  ·  / commands  ·  Ctrl+P browse  ·  Ctrl+C quit",
            Style::default().fg(MUTED),
        )
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn draw_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let border = if !app.connected {
        MUTED
    } else if app.pending {
        WARNING
    } else {
        SECONDARY
    };
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border)),
        area,
    );
    if area.width < 6 || area.height < 3 {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::styled(
            "❯",
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )),
        Rect::new(area.x + 2, area.y + 1, 1, 1),
    );
    let content = Rect::new(
        area.x.saturating_add(4),
        area.y.saturating_add(1),
        area.width.saturating_sub(6),
        area.height.saturating_sub(2),
    );
    if content.width == 0 || content.height == 0 {
        return;
    }
    let layout = app.input.layout(content.width.max(1) as usize);
    let offset = layout
        .cursor_row
        .saturating_sub(content.height.saturating_sub(1) as usize);
    let text = if app.input.is_empty() {
        Text::from(Line::styled(
            "Ask anything…  / for commands",
            Style::default().fg(MUTED),
        ))
    } else {
        Text::from(
            layout
                .segments
                .iter()
                .map(|line| {
                    Line::from(
                        line.iter()
                            .map(|segment| {
                                Span::styled(
                                    segment.text.clone(),
                                    if segment.selected {
                                        Style::default().add_modifier(Modifier::REVERSED)
                                    } else {
                                        Style::default()
                                    },
                                )
                            })
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    };
    frame.render_widget(
        Paragraph::new(text).scroll((offset.min(u16::MAX as usize) as u16, 0)),
        content,
    );
    if app.tab == Tab::Chat && app.approval.is_none() {
        let x = content.x.saturating_add(
            layout
                .cursor_column
                .min(content.width.saturating_sub(1) as usize) as u16,
        );
        let y = content
            .y
            .saturating_add(layout.cursor_row.saturating_sub(offset) as u16);
        frame.set_cursor_position((x, y.min(content.bottom().saturating_sub(1))));
    }
}

pub(super) fn centered(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let width =
        ((u32::from(area.width) * u32::from(width_percent) / 100).max(1) as u16).min(area.width);
    let height =
        ((u32::from(area.height) * u32::from(height_percent) / 100).max(1) as u16).min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::super::app::EntryKind;
    use super::*;
    use ratatui::Terminal;
    use serde_json::json;

    #[test]
    fn welcome_uses_logo_and_terminal_background() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.connected = true;
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("no runner"));
        assert!(rendered.contains("⣿"));
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| matches!(cell.style().bg, None | Some(Color::Reset)))
        );
    }

    #[test]
    fn composer_has_prompt_box_and_suggestions_above() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.runners = vec![json!({ "name": "helper" })];
        app.input.set("/ru");
        let screen = Rect::new(0, 0, 80, 24);
        let rows = screen_rows(screen, &app);
        let backend = ratatui::backend::TestBackend::new(screen.width, screen.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("❯"));
        assert!(rendered.contains("/runner"));
        assert!(rendered.contains("no runner"));
        assert!(!rendered.contains("Message"));
        assert_eq!(suggestion_at(&app, screen, 4, rows[2].y + 1), Some(0));
        assert!(rows[2].y < rows[3].y && rows[3].y < rows[4].y);
    }

    #[test]
    fn composer_highlights_selected_text() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.input.set("hello");
        app.input.begin_selection();
        app.input.left();
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(terminal.backend().buffer().content().iter().any(|cell| {
            cell.symbol() == "o" && cell.style().add_modifier.contains(Modifier::REVERSED)
        }));
    }

    fn rendered(app: &App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn header_and_status_reflect_connection_runner_and_activity() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, Some("review".into()));
        app.connected = true;
        let idle = rendered(&app, 100, 24);
        let header = &idle[..100];
        assert!(header.contains("  review  ·  new chat"));
        assert!(!header.contains("agent.d"));
        assert!(!header.contains("●"));
        assert!(idle.contains("Enter send"));
        app.session = Some("1f9bf89b-0000".into());
        app.session_label = Some("bugfix".into());
        assert!(rendered(&app, 100, 24).contains("  review  ·  bugfix"));
        app.pending = true;
        app.push(EntryKind::User, "go");
        let busy = rendered(&app, 100, 24);
        assert!(busy.contains("working"));
        assert!(!busy.contains("Enter send"));
        app.pending = false;
        app.scroll = 3;
        app.push(EntryKind::Agent, "late");
        assert!(rendered(&app, 100, 24).contains("↓ new messages"));
        app.connected = false;
        assert!(rendered(&app, 100, 24).contains("offline"));
    }
}

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use super::super::app::{App, Tab};
use super::super::presentation;
use super::super::search::Row;
use super::{MUTED, PRIMARY, SECONDARY, centered};

pub(super) fn draw_browser(frame: &mut Frame<'_>, app: &App) {
    let area = centered(frame.area(), 82, 78);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default()
            .title(Span::styled(" browse ", Style::default().fg(PRIMARY)))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(PRIMARY)),
        area,
    );
    let inside = Rect::new(
        area.x + 1,
        area.y + 1,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inside);
    draw_query(frame, app, rows[0]);
    let rows = [rows[1], rows[2], rows[3]];
    frame.render_widget(
        Tabs::new([
            "All", "Runners", "Sessions", "Actions", "Services", "Skills",
        ])
        .select(app.tab.index().saturating_sub(1))
        .divider("  ")
        .style(Style::default().fg(MUTED))
        .highlight_style(Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)),
        rows[0],
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Length(2),
            Constraint::Min(10),
        ])
        .split(rows[1]);
    let matches = app.rows();
    let items: Vec<ListItem> = matches
        .iter()
        .map(|row| ListItem::new(row_line(app.tab, row)))
        .collect();
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(app.selected_index().min(items.len() - 1)));
    }
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("› ")
            .highlight_style(Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)),
        columns[0],
        &mut state,
    );
    let selected = matches.get(app.selected_index().min(matches.len().saturating_sub(1)));
    match selected {
        Some(row) => frame.render_widget(
            Paragraph::new(presentation::value(row.value))
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            columns[2],
        ),
        None => {
            let message = if app.query.is_empty() {
                app.notice
                    .clone()
                    .unwrap_or_else(|| "Nothing here yet. Press F5 to refresh.".into())
            } else {
                format!("Nothing matches \"{}\".", app.query)
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(MUTED))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                Rect::new(
                    rows[1].x,
                    rows[1].y + 1,
                    rows[1].width,
                    rows[1].height.saturating_sub(1),
                ),
            );
        }
    }
    frame.render_widget(
        Paragraph::new(
            " type to search  ·  ↑/↓ select  ·  Enter open  ·  Tab switch  ·  Esc close ",
        )
        .style(Style::default().fg(MUTED)),
        rows[2],
    );
}

fn draw_query(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let line = if app.query.is_empty() {
        Line::from(vec![
            Span::styled(" › ", Style::default().fg(PRIMARY)),
            Span::styled("Search everywhere…", Style::default().fg(MUTED)),
        ])
    } else {
        Line::from(vec![
            Span::styled(" › ", Style::default().fg(PRIMARY)),
            Span::raw(app.query.clone()),
            Span::styled("▏", Style::default().fg(PRIMARY)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn row_line(tab: Tab, row: &Row<'_>) -> Line<'static> {
    let mut spans = Vec::new();
    if tab == Tab::All {
        spans.push(Span::styled(
            format!("{:<9}", section_name(row.section)),
            Style::default().fg(SECONDARY),
        ));
    }
    spans.push(Span::raw(row.label.clone()));
    Line::from(spans)
}

fn section_name(section: Tab) -> &'static str {
    match section {
        Tab::Runners => "runner",
        Tab::Sessions => "session",
        Tab::Actions => "action",
        Tab::Services => "service",
        Tab::Skills => "skill",
        Tab::Chat | Tab::All => "",
    }
}

#[cfg(test)]
mod tests {
    use super::super::draw;
    use super::*;
    use ratatui::Terminal;
    use serde_json::json;

    #[test]
    fn browser_details_are_human_readable() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Runners;
        app.runners = vec![json!({ "name": "review", "model": "mock/test", "skills": ["git"] })];
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Name: review"));
        assert!(!rendered.contains("\"name\""));
        assert!(!rendered.contains("agent.d"));
    }

    #[test]
    fn all_view_prefixes_rows_with_their_section_and_shows_the_query() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::All;
        app.runners = vec![json!({ "name": "helper", "model": "m" })];
        app.actions = vec![json!("git.status")];
        app.set_query("git".into());
        let text = rows(&app, 100, 30).join("\n");
        assert!(text.contains("› git"), "{text}");
        assert!(text.contains("action   git.status"), "{text}");
        assert!(!text.contains("helper"), "{text}");
    }

    fn rows(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn list_column_keeps_a_gap_before_details() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Runners;
        app.runners = vec![json!({ "name": "x".repeat(30), "model": "m" })];
        let line = rows(&app, 100, 30)
            .into_iter()
            .find(|line| line.contains("Model: m"))
            .unwrap();
        assert!(line.contains("xx  Model: m"), "{line}");
    }

    #[test]
    fn empty_list_message_is_centered_and_shows_refresh_errors() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Sessions;
        let line = rows(&app, 100, 30)
            .into_iter()
            .find(|line| line.contains("Nothing here yet"))
            .unwrap();
        let cells: Vec<char> = line.chars().collect();
        let left = cells.iter().position(|c| *c == '│').unwrap();
        let right = cells.iter().rposition(|c| *c == '│').unwrap();
        let inner: String = cells[left + 1..right].iter().collect();
        let leading = inner.len() - inner.trim_start().len();
        let trailing = inner.len() - inner.trim_end().len();
        assert!(leading.abs_diff(trailing) <= 1, "{line}");
        app.notice = Some("sessions.list: daemon has no session store".into());
        let text = rows(&app, 100, 30).join("\n");
        assert!(text.contains("daemon has no session store"));
        assert!(!text.contains("Nothing here yet"));
    }
}

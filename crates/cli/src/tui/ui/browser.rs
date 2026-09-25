use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use serde_json::Value;

use super::super::app::{App, Tab};
use super::super::presentation;
use super::{MUTED, PRIMARY, centered};

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
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inside);
    frame.render_widget(
        Tabs::new(["Runners", "Sessions", "Actions", "Services", "Skills"])
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
    let items: Vec<ListItem> = app
        .list()
        .iter()
        .map(|item| ListItem::new(row(app.tab, item)))
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
    let selected = app
        .list()
        .get(app.selected_index().min(app.list().len().saturating_sub(1)));
    match selected {
        Some(item) => frame.render_widget(
            Paragraph::new(presentation::value(item))
                .style(Style::default().fg(MUTED))
                .wrap(Wrap { trim: false }),
            columns[2],
        ),
        None => {
            let message = app
                .notice
                .clone()
                .unwrap_or_else(|| "Nothing here yet. Press F5 to refresh.".into());
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
        Paragraph::new(" ↑/↓ select  ·  Enter open  ·  Tab switch  ·  Esc close ")
            .style(Style::default().fg(MUTED)),
        rows[2],
    );
}

fn row(tab: Tab, item: &Value) -> String {
    match tab {
        Tab::Runners => format!(
            "{}  ·  {}",
            item["name"].as_str().unwrap_or("?"),
            item["model"].as_str().unwrap_or("default")
        ),
        Tab::Sessions => format!(
            "{}  ·  {} turns",
            item["label"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .unwrap_or("?"),
            item["turn_count"].as_u64().unwrap_or(0)
        ),
        Tab::Actions => item.as_str().unwrap_or("?").to_owned(),
        Tab::Services => format!(
            "{}  ·  {}",
            item["name"].as_str().unwrap_or("?"),
            item["state"].as_str().unwrap_or("?")
        ),
        Tab::Skills => item["name"].as_str().unwrap_or("?").to_owned(),
        Tab::Chat => String::new(),
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

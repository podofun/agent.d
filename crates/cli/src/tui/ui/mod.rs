use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::{App, Tab};

mod approval;
mod browser;
mod conversation;
mod logo;
mod markdown;
mod selection;

pub(super) use conversation::{suggestion_at, toggle_tool, tool_at};
pub(super) use logo::SPIN as LEAF_SPIN;
pub(super) use selection::{Selection, point_at, selected_text};
#[cfg(test)]
pub(super) use {conversation::conversation_inner, selection::Point};

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

/// Where the welcome leaf sits on `screen`, or `None` when it is not shown.
pub(super) fn leaf_area(app: &App, screen: Rect) -> Option<Rect> {
    if app.tab != Tab::Chat || app.pending || !app.entries.is_empty() {
        return None;
    }
    welcome_layout(screen_rows(screen, app)[1]).and_then(|(leaf, _)| leaf)
}

/// The leaf rectangle, when there is room for it, and the row the title
/// goes on. `None` when not even the title and hints fit.
fn welcome_layout(area: Rect) -> Option<(Option<Rect>, u16)> {
    if area.width < logo::MIN_WIDTH || area.height < logo::TEXT_ROWS {
        return None;
    }
    let Some(rows) = logo::tier(area.width, area.height) else {
        let top = area.y + (area.height - logo::TEXT_ROWS) / 2;
        return Some((None, top + 1));
    };
    let top = area.y + (area.height - rows - logo::TEXT_ROWS) / 2;
    let leaf = Rect::new(area.x + (area.width - 2 * rows) / 2, top, 2 * rows, rows);
    Some((Some(leaf), top + rows + 1))
}

fn draw_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some((leaf, title_row)) = welcome_layout(area) else {
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
    };
    if let Some(leaf) = leaf {
        let elapsed = app.leaf_elapsed();
        let (angle, glow) = (logo::angle(elapsed), logo::glow(elapsed));
        frame.render_widget(
            Paragraph::new(logo::render(leaf.height, angle, glow, app.truecolor)),
            leaf,
        );
    }
    let title = "agent.d";
    frame.render_widget(
        Paragraph::new(title).style(Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)),
        Rect::new(
            area.x + area.width.saturating_sub(title.len() as u16) / 2,
            title_row,
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
            Rect::new(area.x, title_row + 2 + index as u16, area.width, 1),
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
    } else if app.selection.is_some() {
        Line::styled(
            "  Ctrl+C copy  ·  Esc clear selection",
            Style::default().fg(SECONDARY),
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

    fn welcome(app: &App, width: u16, height: u16) -> Vec<Vec<ratatui::buffer::Cell>> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].clone()).collect())
            .collect()
    }

    fn leaf_cells(grid: &[Vec<ratatui::buffer::Cell>]) -> usize {
        grid.iter()
            .flatten()
            .filter(|cell| matches!(cell.symbol(), "▀" | "▄"))
            .count()
    }

    fn connected(truecolor: bool) -> App {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.connected = true;
        app.truecolor = truecolor;
        app
    }

    #[test]
    fn welcome_leaf_grows_with_the_terminal() {
        let app = connected(true);
        let small = leaf_cells(&welcome(&app, 80, 33));
        let large = leaf_cells(&welcome(&app, 120, 44));
        assert!(small > 0, "a 33 row terminal shows the leaf");
        assert!(large > small, "{small} cells at 80x33, {large} at 120x44");
        let leaf = leaf_area(&app, Rect::new(0, 0, 200, 80)).unwrap();
        assert_eq!(leaf.height, 12, "even a huge terminal gets a modest leaf");
    }

    #[test]
    fn welcome_leaf_hides_when_there_is_no_room() {
        let app = connected(true);
        let text = |width, height| -> String {
            welcome(&app, width, height)
                .iter()
                .flatten()
                .map(|cell| cell.symbol())
                .collect()
        };
        for (width, height) in [(41, 40), (100, 12), (93, 25)] {
            assert_eq!(
                leaf_cells(&welcome(&app, width, height)),
                0,
                "{width}x{height}"
            );
        }
        let medium = text(93, 25);
        assert!(medium.contains("agent.d") && medium.contains("Type a message to start"));
        assert!(!medium.contains("What would you like to do?"));
        assert!(text(41, 40).contains("What would you like to do?"));
        assert!(text(100, 12).contains("What would you like to do?"));
    }

    #[test]
    fn welcome_title_and_hints_sit_below_the_leaf() {
        let app = connected(true);
        let grid = welcome(&app, 100, 40);
        let rows: Vec<String> = grid
            .iter()
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect();
        let last_leaf = rows
            .iter()
            .rposition(|row| row.contains('▀') || row.contains('▄'))
            .unwrap();
        let title = rows.iter().position(|row| row.contains("agent.d")).unwrap();
        let hint = rows
            .iter()
            .position(|row| row.contains("Type a message to start"))
            .unwrap();
        assert!(
            last_leaf < title && title < hint,
            "{last_leaf} {title} {hint}"
        );
    }

    #[test]
    fn welcome_leaf_is_gray_at_rest_and_coloured_mid_turn() {
        let mut app = connected(true);
        let screen = Rect::new(0, 0, 100, 40);
        let leaf = leaf_area(&app, screen).unwrap();
        let coloured = |app: &App| -> usize {
            let grid = welcome(app, 100, 40);
            (leaf.y..leaf.bottom())
                .flat_map(|y| (leaf.x..leaf.right()).map(move |x| (x, y)))
                .filter(|&(x, y)| {
                    let style = grid[y as usize][x as usize].style();
                    [style.fg, style.bg]
                        .into_iter()
                        .any(|c| matches!(c, Some(Color::Rgb(r, g, b)) if r != g || g != b))
                })
                .count()
        };
        assert_eq!(coloured(&app), 0);
        // 15% in: still facing the viewer and already mostly coloured.
        app.leaf_spin = Some(std::time::Instant::now() - LEAF_SPIN * 3 / 20);
        assert!(
            coloured(&app) > 20,
            "the turning leaf shows the logo colours"
        );
    }

    #[test]
    fn welcome_keeps_the_terminal_background_around_the_leaf() {
        let app = connected(true);
        for cell in welcome(&app, 100, 40).iter().flatten() {
            if cell.symbol() != "▀" {
                assert!(matches!(cell.style().bg, None | Some(Color::Reset)));
            }
        }
    }

    #[test]
    fn welcome_falls_back_to_braille_without_true_colour() {
        let app = connected(false);
        let grid = welcome(&app, 100, 40);
        assert_eq!(leaf_cells(&grid), 0);
        assert!(grid.iter().flatten().any(|cell| {
            let c = cell.symbol().chars().next().unwrap_or(' ');
            ('\u{2801}'..='\u{28FF}').contains(&c)
        }));
        let leaf = leaf_area(&app, Rect::new(0, 0, 100, 40)).unwrap();
        for y in leaf.y..leaf.bottom() {
            for x in leaf.x..leaf.right() {
                let style = grid[y as usize][x as usize].style();
                assert!(!matches!(style.fg, Some(Color::Rgb(..))), "({x}, {y})");
            }
        }
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

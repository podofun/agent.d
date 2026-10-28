use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};

use unicode_width::UnicodeWidthStr;

use super::app::{App, Request, Tab};

mod approval;
mod browser;
mod conversation;
mod highlight;
mod logo;
mod markdown;
mod script;
mod selection;

#[cfg(test)]
pub(super) use conversation::BUILDS;
pub(super) use conversation::conversation_inner;
pub(super) use conversation::{
    ConversationCache, clickable_at, max_scroll, suggestion_at, toggle_tool, tool_at,
};
pub(super) use logo::SPIN as LEAF_SPIN;
pub(super) use script::{
    Placed, element_lines, hit as script_hit, point_at as script_point_at,
    scroll_at as script_scroll_at, scroll_region as script_scroll_region,
    terminals as script_terminals,
};
#[cfg(test)]
pub(super) use selection::Point;
pub(super) use selection::{Region, Selection, point_at, selected_text};

pub(super) const PRIMARY: Color = Color::Rgb(177, 124, 245);
pub(super) const SECONDARY: Color = Color::Rgb(122, 180, 234);
pub(super) const MUTED: Color = Color::Rgb(138, 143, 152);
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
    // Outside a run, such as while a chat loads, it turns with the clock.
    let millis = match app.started_at() {
        Some(started) => started.elapsed().as_millis(),
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_millis()),
    };
    let tick = millis / 120;
    SPINNER[(tick % SPINNER.len() as u128) as usize]
}

pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    if app.script.custom() {
        script::draw(frame, app);
    } else {
        let area = frame.area();
        let rows = screen_rows(area, app);
        draw_header(frame, app, rows[0], 0);
        conversation::draw_conversation(frame, app, rows[1]);
        conversation::draw_suggestions(frame, app, rows[2]);
        draw_composer(frame, app, rows[3], None);
        draw_status(frame, app, rows[4]);
    }
    if app.tab != Tab::Chat {
        browser::draw_browser(frame, app);
    }
    if let Some(request) = &app.approval {
        approval::draw_approval(frame, request, app.approval_scroll);
    }
}

pub(super) fn screen_rows(area: Rect, app: &App) -> [Rect; 5] {
    if let Some(rows) = script::part_rects(app, area) {
        return rows;
    }
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
    let available = area.height.saturating_sub(2 + 4 + composer_height + 2);
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
            Constraint::Length(2),
        ])
        .split(area);
    [rows[0], rows[1], rows[2], rows[3], rows[4]]
}

/// One quiet line: a dot that shows the chat's state, the chat's name,
/// then what is worth knowing right now, in muted text. The last `right`
/// columns belong to what scripts put there. The row below stays empty.
fn draw_header(frame: &mut Frame<'_>, app: &App, area: Rect, right: u16) {
    if area.height == 0 {
        return;
    }
    // Not offline yet while the first lists load.
    let offline = !app.connected && !app.loading(Request::Lists);
    let (dot, dot_color) = if offline {
        ("●", ERROR)
    } else if app.pending {
        (spinner_frame(app), WARNING)
    } else {
        ("●", PRIMARY)
    };
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(dot, Style::default().fg(dot_color)),
        Span::raw(" "),
    ];
    match app.session_label.as_deref() {
        Some(name) => spans.push(Span::styled(
            name.to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        None => spans.push(Span::styled("new chat", Style::default().fg(MUTED))),
    }
    let mut notes: Vec<(&str, Color)> = Vec::new();
    if offline {
        notes.push(("offline", ERROR));
    }
    if app.selection.is_some_and(|selection| !selection.is_click()) {
        notes.push(("Ctrl+C copies, Esc clears", MUTED));
    } else if app.quit_armed() {
        notes.push(("Ctrl+C again to quit", WARNING));
    }
    if app.has_unread() {
        notes.push(("↓ new", SECONDARY));
    }
    for (note, color) in notes {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(note, Style::default().fg(color)));
    }
    // Two columns keep script elements off the name.
    let right = if right > 0 { right + 2 } else { 0 };
    let line = Rect::new(area.x, area.y, area.width.saturating_sub(right), 1);
    frame.render_widget(Paragraph::new(Line::from(spans)), line);
}

/// Where the welcome leaf sits on `screen`, or `None` when it is not shown.
pub(super) fn leaf_area(app: &App, screen: Rect) -> Option<Rect> {
    if app.tab != Tab::Chat || app.pending || !app.entries.is_empty() || !app.queue.is_empty() {
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
    let hints: &[&str] = if !app.connected && app.loading(Request::Lists) {
        &["Connecting…"]
    } else if app.connected {
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

/// Under the input: folder and branch, with runner and model on the right,
/// and the approval mode below them. When the line is too narrow, the model
/// shrinks first, then the branch, then the runner, then the folder.
fn draw_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let runner = app.runner.as_deref().unwrap_or("no runner");
    let branch = app.branch.as_deref().unwrap_or_default();
    let model = app.model().unwrap_or_default();
    // Two columns of padding at each edge and two between the sides.
    let room = (area.width as usize).saturating_sub(6);
    let [folder, runner, branch, model] = fit_parts([&app.workdir, runner, branch, model], room);
    let mut left = vec![
        Span::raw("  "),
        Span::styled(folder, Style::default().add_modifier(Modifier::BOLD)),
    ];
    if !branch.is_empty() {
        left.push(Span::raw(" "));
        left.push(Span::styled(
            branch,
            Style::default().add_modifier(Modifier::ITALIC),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(left)), Rect { height: 1, ..area });
    let runner_style = if app.runner.is_some() {
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(MUTED)
    };
    let mut right = vec![Span::styled(runner, runner_style)];
    if !model.is_empty() {
        right.push(Span::raw(" "));
        right.push(Span::styled(model, Style::default().fg(MUTED)));
    }
    right.push(Span::raw("  "));
    frame.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        Rect { height: 1, ..area },
    );
    let (mode, color) = match app.approval_mode {
        Some("classifier") => ("Auto mode", WARNING),
        Some("nobody") => ("No approvals", ERROR),
        Some(_) => ("Manual approval", Color::Reset),
        None => ("Default approval", Color::Reset),
    };
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                format!("  {mode}"),
                Style::default().fg(color),
            )),
            Rect::new(area.x, area.y + 1, area.width, 1),
        );
    }
}

/// Share `room` columns between parts given in order of importance, with one
/// column between parts. A part cut short ends in `…`; one with no room left
/// is dropped.
fn fit_parts<const N: usize>(parts: [&str; N], room: usize) -> [String; N] {
    let mut left = room;
    parts.map(|part| {
        let width = part.width();
        if part.is_empty() || left < 2 {
            return String::new();
        }
        let fitted = truncate(part, left);
        left = left.saturating_sub(width.min(left) + 1);
        fitted
    })
}

/// `text` cut to `width` columns, ending in `…` when it was longer.
pub(super) fn truncate(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    for grapheme in text.graphemes(true) {
        if out.width() + grapheme.width() + 1 > width {
            break;
        }
        out.push_str(grapheme);
    }
    out.push('…');
    out
}

/// The prompt. A script may colour its border.
fn draw_composer(frame: &mut Frame<'_>, app: &App, area: Rect, border: Option<Color>) {
    let border = border.unwrap_or(if !app.connected {
        MUTED
    } else if app.pending {
        WARNING
    } else {
        SECONDARY
    });
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
        let small = leaf_cells(&welcome(&app, 80, 34));
        let large = leaf_cells(&welcome(&app, 120, 44));
        assert!(small > 0, "a 34 row terminal shows the leaf");
        assert!(large > small, "{small} cells at 80x34, {large} at 120x44");
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
    fn a_narrow_status_line_shrinks_the_model_first_and_never_overlaps() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, Some("quick_chat".into()));
        app.workdir = "agent.d".into();
        app.branch = Some("feat/chat-queue-and-modes".into());
        app.runners =
            vec![serde_json::json!({ "name": "quick_chat", "model": "deepseek/deepseek-flash" })];
        for width in [20u16, 40, 60, 100] {
            let screen: Vec<char> = rendered(&app, width, 24).chars().collect();
            let from = screen.len() - 2 * width as usize;
            let line: String = screen[from..from + width as usize].iter().collect();
            assert!(line.starts_with("  agent.d"), "{width}: {line}");
            assert!(line.contains("quick"), "{width}: {line}");
            assert!(
                line.contains("  quick") || line.contains(" quick"),
                "{width}: {line}"
            );
        }
        assert_eq!(
            fit_parts(["agent.d", "quick_chat", "feat/chat", "deepseek/flash"], 32),
            ["agent.d", "quick_chat", "feat/chat", "de…"]
        );
        assert_eq!(truncate("deepseek", 4), "dee…");
    }

    #[test]
    fn header_shows_the_chat_and_status_shows_where_it_runs() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, Some("review".into()));
        app.connected = true;
        app.workdir = "agent.d".into();
        app.branch = Some("main".into());
        app.runners = vec![serde_json::json!({ "name": "review", "model": "anthropic/claude" })];
        let header = |app: &App| rendered(app, 100, 24).chars().take(100).collect::<String>();
        let status = |app: &App| {
            let screen: Vec<char> = rendered(app, 100, 24).chars().collect();
            let row = |from: usize| screen[from..from + 100].iter().collect::<String>();
            (row(screen.len() - 200), row(screen.len() - 100))
        };
        assert!(header(&app).starts_with("  ● new chat"), "{}", header(&app));
        assert!(!header(&app).contains("review"));
        let (place, mode) = status(&app);
        assert!(place.starts_with("  agent.d main "), "{place}");
        assert!(place.ends_with("review anthropic/claude  "), "{place}");
        assert_eq!(mode.trim_end(), "  Default approval");
        app.session_label = Some("bugfix".into());
        assert!(header(&app).starts_with("  ● bugfix"));
        app.pending = true;
        app.push(EntryKind::User, "go");
        let busy = rendered(&app, 100, 24);
        // The dot turns while the agent works.
        assert!(!header(&app).starts_with("  ●"), "{}", header(&app));
        assert!(header(&app).contains(" bugfix"));
        // Working shows inline in the conversation, not under the input.
        assert!(busy.contains("working"));
        assert!(!status(&app).0.contains("working"));
        app.approval_mode = Some("classifier");
        assert_eq!(status(&app).1.trim_end(), "  Auto mode");
        assert!(!header(&app).contains("auto"));
        // Short status shows in the conversation, and errors join it there.
        app.notice = Some("Cancelling…\nmore".into());
        let screen = rendered(&app, 100, 24);
        assert!(!header(&app).contains("Cancelling"));
        assert!(screen.contains("Cancelling") && !screen.contains("more"));
        app.notice = None;
        app.fail("Approval connection closed");
        assert!(!header(&app).contains("Approval connection"));
        assert!(rendered(&app, 100, 24).contains("Approval connection closed"));
        app.selection = Some(super::Selection {
            region: super::Region::Conversation,
            anchor: super::Point { line: 0, col: 0 },
            head: super::Point { line: 0, col: 1 },
        });
        assert!(header(&app).contains("Ctrl+C copies, Esc clears"));
        app.connected = false;
        assert!(header(&app).contains("offline"));
        // No rule under the header: the row below it is empty.
        let second: String = rendered(&app, 100, 24)
            .chars()
            .skip(100)
            .take(100)
            .collect();
        assert_eq!(second.trim(), "");
    }
}

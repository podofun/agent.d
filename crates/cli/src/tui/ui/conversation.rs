use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::super::app::{App, Entry, EntryKind};
use super::super::commands;
use super::{
    ERROR, MUTED, PRIMARY, SECONDARY, SUCCESS, WARNING, markdown, palette, screen_rows, selection,
    spinner_frame,
};

pub(super) fn draw_conversation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.entries.is_empty() && !app.pending {
        super::draw_welcome(frame, app, area);
        return;
    }
    let inner = conversation_inner(area);
    let mut conversation = conversation_for(app, inner.width.saturating_sub(1).max(1) as usize);
    if let Some(selection) = &app.selection {
        selection::highlight(&mut conversation.lines, &conversation.selectable, selection);
    }
    let offset = conversation.offset(inner.height, app.scroll);
    frame.render_widget(
        Paragraph::new(conversation.lines).scroll((offset.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
}

pub(super) struct Conversation {
    pub(super) lines: Vec<Line<'static>>,
    /// Selectable column range of each line, parallel to `lines`.
    pub(super) selectable: Vec<std::ops::Range<usize>>,
    tool_headers: Vec<(usize, usize)>,
}

/// The rendered conversation at `width` text columns.
pub(super) fn conversation_for(app: &App, width: usize) -> Conversation {
    conversation_lines(app, width)
}

impl Conversation {
    pub(super) fn offset(&self, height: u16, scroll: usize) -> usize {
        self.lines
            .len()
            .saturating_sub(height as usize)
            .saturating_sub(scroll)
    }
}

pub(in crate::tui) fn conversation_inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(2),
        area.y,
        area.width.saturating_sub(4),
        area.height,
    )
}

const GUTTER: usize = 3;

fn gutter(glyph: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!("{glyph}  "),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn indented(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(" ".repeat(GUTTER)));
    line
}

fn with_gutter(glyph: &str, color: Color, body: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut body = body.into_iter();
    let mut lines = Vec::new();
    if let Some(mut first) = body.next() {
        first.spans.insert(0, gutter(glyph, color));
        lines.push(first);
    }
    lines.extend(body.map(indented));
    lines
}

fn conversation_lines(app: &App, width: usize) -> Conversation {
    let body_width = width.saturating_sub(GUTTER).max(1);
    let mut lines = Vec::new();
    let mut selectable = Vec::new();
    let mut tool_headers = Vec::new();
    for (index, entry) in app.entries.iter().enumerate() {
        if entry.is_expandable_tool() {
            tool_headers.push((lines.len(), index));
        }
        let block = entry.lines(body_width, || entry_lines(entry, body_width));
        selectable.extend(
            block
                .iter()
                .enumerate()
                .map(|(row, line)| selectable_range(entry, row, line)),
        );
        lines.extend(block);
        lines.push(Line::raw(""));
        selectable.push(0..0);
    }
    if app.pending {
        let live = markdown::render(&app.streamed, body_width, palette());
        let live = with_gutter(spinner_frame(app), WARNING, live);
        // The live block changes every tick, so it is never selectable.
        selectable.extend(live.iter().map(|_| 0..0));
        lines.extend(live);
    }
    Conversation {
        lines,
        selectable,
        tool_headers,
    }
}

/// Columns of `line` (row `row` of its entry) that hold the entry's own
/// text: past the gutter or indent, past the `│ ` prefix on tool output,
/// and before the collapsed/expanded summary on a tool row.
fn selectable_range(entry: &Entry, row: usize, line: &Line<'static>) -> std::ops::Range<usize> {
    let width = line.width();
    match entry.kind {
        EntryKind::Tool if row == 0 => {
            let title = line
                .spans
                .get(1)
                .map_or(0, |span| span.content.trim_end().width());
            GUTTER.min(width)..(GUTTER + title).min(width)
        }
        EntryKind::Tool => (GUTTER + 2).min(width)..width,
        _ => GUTTER.min(width)..width,
    }
}

fn entry_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let body = if entry.markdown {
        markdown::render(&entry.text, width, palette())
    } else {
        markdown::wrap_plain(&entry.text, width)
    };
    match entry.kind {
        EntryKind::User => with_gutter("›", SECONDARY, emphasized(body)),
        EntryKind::Agent => with_gutter("◆", PRIMARY, body),
        EntryKind::Info => with_gutter("i", SUCCESS, body),
        EntryKind::Error => with_gutter("!", ERROR, body),
        EntryKind::Tool => tool_lines(entry, width),
    }
}

fn emphasized(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| line.patch_style(Modifier::BOLD))
        .collect()
}

fn tool_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let count = entry.body.lines().count();
    let summary = if !entry.is_expandable_tool() {
        String::new()
    } else if entry.expanded {
        "▾".to_owned()
    } else {
        format!("▸ {count} line{}", if count == 1 { "" } else { "s" })
    };
    let title_width = width.saturating_sub(summary.width() + 1);
    let mut title: String = entry.text.graphemes(true).take(title_width).collect();
    title.push_str(&" ".repeat(title_width.saturating_sub(title.width()) + 1));
    let mut lines = vec![Line::from(vec![
        gutter("✦", WARNING),
        Span::styled(title, Style::default().fg(SECONDARY)),
        Span::styled(summary, Style::default().fg(MUTED)),
    ])];
    if entry.expanded {
        for mut part in markdown::wrap_plain(&entry.body, width.saturating_sub(2)) {
            part.spans
                .insert(0, Span::styled("│ ", Style::default().fg(WARNING)));
            lines.push(indented(part));
        }
    }
    lines
}

pub(in crate::tui) fn tool_at(app: &App, screen: Rect, x: u16, y: u16) -> Option<usize> {
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    if x < inner.x || x >= inner.right() || y < inner.y || y >= inner.bottom() {
        return None;
    }
    let conversation = conversation_lines(app, inner.width.saturating_sub(1).max(1) as usize);
    let line = conversation.offset(inner.height, app.scroll) + usize::from(y - inner.y);
    conversation
        .tool_headers
        .into_iter()
        .find_map(|(header, index)| (line == header).then_some(index))
}

pub(in crate::tui) fn toggle_tool(app: &mut App, screen: Rect, index: usize) {
    if !app
        .entries
        .get(index)
        .is_some_and(Entry::is_expandable_tool)
    {
        return;
    }
    let expanded = !app.entries[index].expanded;
    app.entries[index].set_expanded(expanded);
    app.selection = None;
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    let conversation = conversation_lines(app, inner.width.saturating_sub(1).max(1) as usize);
    if let Some((header, _)) = conversation
        .tool_headers
        .iter()
        .find(|(_, entry)| *entry == index)
    {
        app.scroll = conversation
            .lines
            .len()
            .saturating_sub(inner.height as usize)
            .saturating_sub(*header);
    }
}

pub(super) fn draw_suggestions(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.height < 3 || area.width < 4 {
        return;
    }
    let suggestions = app.suggestions();
    if suggestions.is_empty() {
        return;
    }
    frame.render_widget(
        Block::default()
            .title(commands::picker_title(app.input.text()).unwrap_or(" Commands "))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED)),
        area,
    );
    let selected = app.suggestion_index.min(suggestions.len() - 1);
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let first = selected.saturating_sub(visible - 1);
    let items = suggestions
        .iter()
        .skip(first)
        .take(visible)
        .map(|suggestion| {
            ListItem::new(Line::from(vec![
                Span::styled(suggestion.label.clone(), Style::default().fg(PRIMARY)),
                Span::styled(
                    format!("  {}", suggestion.description),
                    Style::default().fg(Color::Gray),
                ),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if suggestions[selected].completion.is_some() {
        state.select(Some(selected - first));
    }
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("❯ ")
            .highlight_style(Style::default().add_modifier(Modifier::BOLD)),
        Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        ),
        &mut state,
    );
}

pub(in crate::tui) fn suggestion_at(app: &App, screen: Rect, x: u16, y: u16) -> Option<usize> {
    let area = screen_rows(screen, app)[2];
    if x <= area.x
        || x >= area.right().saturating_sub(1)
        || y <= area.y
        || y >= area.bottom().saturating_sub(1)
    {
        return None;
    }
    let suggestions = app.suggestions();
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let selected = app
        .suggestion_index
        .min(suggestions.len().saturating_sub(1));
    let index = selected.saturating_sub(visible - 1) + usize::from(y - area.y - 1);
    suggestions
        .get(index)
        .and_then(|item| item.completion.as_ref().map(|_| index))
}

#[cfg(test)]
mod tests {
    use super::super::draw;
    use super::*;
    use ratatui::Terminal;
    use serde_json::json;

    #[test]
    fn picker_scrolls_with_selection_and_click_maps_to_visible_item() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.runners = (0..8)
            .map(|index| json!({ "name": format!("runner-{index}") }))
            .collect();
        app.input.set("/runner ");
        app.suggestion_index = 7;
        let screen = Rect::new(0, 0, 80, 24);
        let menu = screen_rows(screen, &app)[2];
        assert_eq!(suggestion_at(&app, screen, menu.x + 2, menu.y + 1), Some(2));
        assert_eq!(
            suggestion_at(&app, screen, menu.x + 2, menu.bottom() - 2),
            Some(7)
        );
        let backend = ratatui::backend::TestBackend::new(screen.width, screen.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Runners · type to search"));
        assert!(rendered.contains("runner-7"));
        assert!(!rendered.contains("runner-0"));
    }

    #[test]
    fn tool_output_is_collapsed_quoted_and_clickable() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("git.status", "yes it worked\ndiff:\nmodified: README.md");
        let screen = Rect::new(0, 0, 80, 20);
        let backend = ratatui::backend::TestBackend::new(screen.width, screen.height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("✦  git.status"));
        assert!(rendered.contains("3 lines"));
        assert!(!rendered.contains("modified: README.md"));

        let inner = conversation_inner(screen_rows(screen, &app)[1]);
        let conversation = conversation_lines(&app, inner.width.saturating_sub(1) as usize);
        let header = conversation.tool_headers[0].0 - conversation.offset(inner.height, app.scroll);
        assert_eq!(
            tool_at(&app, screen, inner.x, inner.y + header as u16),
            Some(0)
        );
        toggle_tool(&mut app, screen, 0);
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("│ modified: README.md"));
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
    fn entries_use_gutter_glyphs_and_formatted_text() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "hello **there**");
        app.push(EntryKind::Agent, "Hi **friend**\n\n- item");
        app.push_tool("git.status", "one\ntwo\nthree");
        let text = rendered(&app, 60, 20);
        assert!(text.contains("›  hello **there**"));
        assert!(text.contains("◆  Hi friend"));
        assert!(text.contains("• item"));
        assert!(text.contains("✦  git.status"));
        assert!(text.contains("3 lines"));
        assert!(!text.contains("you"));
        assert!(!text.contains("agent "));
    }

    #[test]
    fn user_messages_stand_out_in_bold() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "hello there");
        app.push(EntryKind::Agent, "reply");
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let cell_named = |needle: &str| {
            let text: String = (0..60).map(|x| buffer[(x, 2)].symbol()).collect();
            let x = text.find(needle).unwrap() as u16;
            buffer[(x, 2)].clone()
        };
        assert!(
            cell_named("hello")
                .style()
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let reply: String = (0..60).map(|x| buffer[(x, 4)].symbol()).collect();
        let x = reply.find("reply").unwrap() as u16;
        assert!(!buffer[(x, 4)].style().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn help_renders_headings_and_key_names_with_style() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_markdown(EntryKind::Info, "## Chat\n\n- `Enter` sends");
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16| -> String { (0..60).map(|x| buffer[(x, y)].symbol()).collect() };
        let heading = row(2);
        let x = heading.find("Chat").unwrap() as u16;
        assert!(buffer[(x, 2)].style().add_modifier.contains(Modifier::BOLD));
        let bullet = row(4);
        assert!(bullet.contains("• Enter sends"), "{bullet}");
        let x = bullet.find("Enter").unwrap() as u16;
        assert_eq!(buffer[(x, 4)].style().fg, Some(SECONDARY));
    }

    #[test]
    fn info_entries_keep_their_line_structure() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::Info, "Name: review\nSkills\n  • git");
        let text = rendered(&app, 60, 12);
        assert!(text.contains("i  Name: review "));
        assert!(text.contains("   Skills "));
        assert!(text.contains("     • git"));
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::Agent, "some text that is long enough to wrap");
        app.push_tool("git.status", "x");
        for width in [1u16, 5, 8, 11] {
            let _ = rendered(&app, width, 6);
        }
    }
}

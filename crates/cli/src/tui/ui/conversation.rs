use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::super::app::{App, Entry, EntryKind, ToolRow};
use super::super::commands;
use super::super::presentation;
use super::{
    ERROR, MUTED, PRIMARY, SECONDARY, SUCCESS, WARNING, markdown, palette, screen_rows, selection,
    spinner_frame,
};

pub(super) fn draw_conversation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.entries.is_empty() && app.queue.is_empty() && !app.pending {
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
    tool_headers: Vec<(usize, ToolRow)>,
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
    let mut index = 0;
    while index < app.entries.len() {
        let entry = &app.entries[index];
        let run = if matches!(entry.kind, EntryKind::Tool) {
            app.tool_run(index)
        } else {
            index..index + 1
        };
        if run.len() >= 2 {
            // A run of tool calls reads as one row until it is opened.
            let calls = &app.entries[run.clone()];
            let open = entry.group_open;
            tool_headers.push((lines.len(), ToolRow::Group(run.start)));
            let header = group_header(app, calls, open, body_width);
            selectable.push(GUTTER.min(width)..header.width().min(width));
            lines.push(header);
            if open {
                for (offset, call) in calls.iter().enumerate() {
                    if call.is_expandable_tool() {
                        tool_headers.push((lines.len(), ToolRow::Entry(run.start + offset)));
                    }
                    let block = call.lines(body_width, || entry_lines(call, body_width));
                    selectable.extend(
                        block
                            .iter()
                            .enumerate()
                            .map(|(row, line)| selectable_range(call, row, line))
                            .map(|range| range.start + GUTTER..range.end + GUTTER),
                    );
                    lines.extend(block.into_iter().map(indented));
                }
            }
        } else {
            if entry.is_expandable_tool() {
                tool_headers.push((lines.len(), ToolRow::Entry(index)));
            }
            let block = entry.lines(body_width, || entry_lines(entry, body_width));
            selectable.extend(
                block
                    .iter()
                    .enumerate()
                    .map(|(row, line)| selectable_range(entry, row, line)),
            );
            lines.extend(block);
        }
        lines.push(Line::raw(""));
        selectable.push(0..0);
        index = run.end;
    }
    if app.pending {
        // The live reply, or while nothing has arrived yet, how long the
        // agent has been working.
        let live = if app.streamed.trim().is_empty() {
            let elapsed = app
                .started_at()
                .map_or(0, |started| started.elapsed().as_secs());
            vec![Line::styled(
                format!("working  {elapsed}s"),
                Style::default().fg(MUTED),
            )]
        } else {
            markdown::render(&app.streamed, body_width, palette())
        };
        let live = with_gutter(spinner_frame(app), WARNING, live);
        // The live block changes every tick, so it is never selectable.
        selectable.extend(live.iter().map(|_| 0..0));
        lines.extend(live);
    }
    for queued in &app.queue {
        lines.push(Line::raw(""));
        selectable.push(0..0);
        let mut body = emphasized(markdown::wrap_plain(queued, body_width));
        if let Some(last) = body.last_mut() {
            let tag = if app.queue_held {
                "  not sent"
            } else {
                "  queued"
            };
            last.spans
                .push(Span::styled(tag, Style::default().fg(MUTED)));
        }
        let queued = with_gutter("›", SECONDARY, body);
        selectable.extend(queued.iter().map(|_| 0..0));
        lines.extend(queued);
    }
    Conversation {
        lines,
        selectable,
        tool_headers,
    }
}

/// "Used 14 actions and 1 tool": a call is an action when its name is a
/// registered action, and a tool otherwise.
fn used_label(app: &App, calls: &[Entry]) -> String {
    let actions = calls
        .iter()
        .filter(|call| {
            app.actions
                .iter()
                .any(|action| action.as_str() == Some(call.text.as_str()))
        })
        .count();
    let tools = calls.len() - actions;
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    match (actions, tools) {
        (0, tools) => format!("Used {}", plural(tools, "tool")),
        (actions, 0) => format!("Used {}", plural(actions, "action")),
        (actions, tools) => format!(
            "Used {} and {}",
            plural(actions, "action"),
            plural(tools, "tool")
        ),
    }
}

fn group_header(app: &App, calls: &[Entry], open: bool, width: usize) -> Line<'static> {
    let label = used_label(app, calls);
    let marker = if open { "▾" } else { "▸" };
    let title: String = label
        .graphemes(true)
        .take(width.saturating_sub(2))
        .collect();
    Line::from(vec![
        gutter("✦", WARNING),
        Span::styled(format!("{title} "), Style::default().fg(SECONDARY)),
        Span::styled(marker, Style::default().fg(MUTED)),
    ])
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
        EntryKind::Error => with_gutter(
            "!",
            ERROR,
            body.into_iter()
                .map(|line| line.patch_style(Style::default().fg(ERROR)))
                .collect(),
        ),
        EntryKind::Tool => tool_lines(entry, width),
        EntryKind::Setting => with_gutter(
            "•",
            SECONDARY,
            body.into_iter()
                .map(|line| line.patch_style(Style::default().fg(MUTED)))
                .collect(),
        ),
    }
}

fn emphasized(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| line.patch_style(Modifier::BOLD))
        .collect()
}

/// A call's row: its name, then each short argument (paths, patterns,
/// commands) in full. Collapsed, it previews the first lines of the long
/// argument it carries (such as content being written), or else of its
/// output. Expanded, it shows every long argument and the whole output.
fn tool_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
    let (short, long) = split_input(entry.input.as_ref());
    let marker = match (entry.is_expandable_tool(), entry.expanded) {
        (false, _) => "",
        (true, true) => " ▾",
        (true, false) => " ▸",
    };
    let mut spans = vec![
        Span::styled(entry.text.clone(), Style::default().fg(SECONDARY)),
        Span::styled(marker, Style::default().fg(MUTED)),
    ];
    for (key, value) in short {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("{key} "), Style::default().fg(MUTED)));
        spans.push(Span::raw(value));
    }
    let mut lines = with_gutter("✦", WARNING, markdown::wrap_spans(spans, width));
    let inner = width.saturating_sub(2);
    if !entry.expanded {
        let preview = long.first().map_or(entry.body.as_str(), |(_, text)| text);
        lines.extend(output_preview(preview, inner));
        return lines;
    }
    let block = |lines: &mut Vec<Line<'static>>, label: &str, text: &str, bar: Color| {
        if !label.is_empty() {
            lines.push(indented(Line::styled(
                format!("  {label}"),
                Style::default().fg(MUTED),
            )));
        }
        for mut part in markdown::wrap_plain(text, inner) {
            part.spans
                .insert(0, Span::styled("│ ", Style::default().fg(bar)));
            lines.push(indented(part));
        }
    };
    for (key, text) in &long {
        block(&mut lines, key, text, MUTED);
    }
    if !entry.body.is_empty() {
        let label = if long.is_empty() { "" } else { "result" };
        block(&mut lines, label, &entry.body, WARNING);
    }
    lines
}

/// Named argument values, in the order the call gave them.
type Fields = Vec<(String, String)>;

/// A call's arguments, split into short ones that fit on its row and long
/// ones (multi-line text, nested values) shown as blocks.
fn split_input(input: Option<&Value>) -> (Fields, Fields) {
    const SHORT: usize = 200;
    let (mut short, mut long) = (Vec::new(), Vec::new());
    let Some(input) = input else {
        return (short, long);
    };
    let Value::Object(fields) = input else {
        long.push(("input".to_owned(), presentation::value(input)));
        return (short, long);
    };
    let scalar = |value: &Value| match value {
        Value::String(text) if !text.contains('\n') && text.chars().count() <= SHORT => {
            Some(text.clone())
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    };
    for (key, value) in fields {
        let fitted = match value {
            Value::Array(items) => items
                .iter()
                .map(scalar)
                .collect::<Option<Vec<_>>>()
                .map(|items| items.join(" "))
                .filter(|joined| joined.chars().count() <= SHORT),
            Value::Null => continue,
            other => scalar(other),
        };
        match fitted {
            Some(text) => short.push((key.clone(), text)),
            None => long.push((
                key.clone(),
                value
                    .as_str()
                    .map_or_else(|| presentation::value(value), str::to_owned),
            )),
        }
    }
    (short, long)
}

/// The first lines of a collapsed call's text, muted and cut to one row each.
fn output_preview(body: &str, width: usize) -> Vec<Line<'static>> {
    const ROWS: usize = 3;
    body.lines()
        .map(str::trim_end)
        .skip_while(|line| line.trim().is_empty())
        .take(ROWS)
        .map(|line| {
            let mut text: String = line.graphemes(true).take(width).collect();
            if text.width() < line.width() {
                text = text
                    .graphemes(true)
                    .take(width.saturating_sub(1))
                    .collect::<String>()
                    + "…";
            }
            indented(Line::from(vec![
                Span::raw("  "),
                Span::styled(text, Style::default().fg(MUTED)),
            ]))
        })
        .collect()
}

/// How far the conversation can scroll up on `screen`.
pub(in crate::tui) fn max_scroll(app: &App, screen: Rect) -> usize {
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    conversation_lines(app, inner.width.saturating_sub(1).max(1) as usize)
        .lines
        .len()
        .saturating_sub(inner.height as usize)
}

pub(in crate::tui) fn tool_at(app: &App, screen: Rect, x: u16, y: u16) -> Option<ToolRow> {
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    if x < inner.x || x >= inner.right() || y < inner.y || y >= inner.bottom() {
        return None;
    }
    let conversation = conversation_lines(app, inner.width.saturating_sub(1).max(1) as usize);
    let line = conversation.offset(inner.height, app.scroll) + usize::from(y - inner.y);
    conversation
        .tool_headers
        .into_iter()
        .find_map(|(header, row)| (line == header).then_some(row))
}

/// Open or close a tool row, or a run of them, and keep it in view.
pub(in crate::tui) fn toggle_tool(app: &mut App, screen: Rect, row: ToolRow) {
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    let width = inner.width.saturating_sub(1).max(1) as usize;
    let before = conversation_lines(app, width).offset(inner.height, app.scroll);
    match row {
        ToolRow::Entry(index) => {
            if !app
                .entries
                .get(index)
                .is_some_and(Entry::is_expandable_tool)
            {
                return;
            }
            let expanded = !app.entries[index].expanded;
            app.entries[index].set_expanded(expanded);
        }
        ToolRow::Group(index) => {
            let Some(entry) = app.entries.get_mut(index) else {
                return;
            };
            entry.group_open = !entry.group_open;
        }
    }
    app.selection = None;
    let after = conversation_lines(app, width);
    let Some(&(header, _)) = after.tool_headers.iter().find(|(_, target)| *target == row) else {
        return;
    };
    // Keep the row where it was on screen; a row out of view moves to the top.
    let height = inner.height as usize;
    let row_on_screen = header
        .checked_sub(before)
        .filter(|row| *row < height)
        .unwrap_or(0);
    app.scroll = after
        .lines
        .len()
        .saturating_sub(height)
        .saturating_sub(header - row_on_screen);
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
        assert!(rendered.contains("✦  git.status ▸"));
        // Collapsed output shows as a short preview, not as the quoted body.
        assert!(rendered.contains("modified: README.md"));
        assert!(!rendered.contains("│ modified"));

        let inner = conversation_inner(screen_rows(screen, &app)[1]);
        let conversation = conversation_lines(&app, inner.width.saturating_sub(1) as usize);
        let header = conversation.tool_headers[0].0 - conversation.offset(inner.height, app.scroll);
        assert_eq!(
            tool_at(&app, screen, inner.x, inner.y + header as u16),
            Some(ToolRow::Entry(0))
        );
        toggle_tool(&mut app, screen, ToolRow::Entry(0));
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

    #[test]
    fn opening_a_call_keeps_its_row_in_place() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        for turn in 0..10 {
            app.push(EntryKind::User, format!("question {turn}"));
        }
        app.push_tool("git.diff", "line\n".repeat(30));
        app.push(EntryKind::Agent, "done");
        let screen = Rect::new(0, 0, 80, 20);
        let inner = conversation_inner(screen_rows(screen, &app)[1]);
        let width = inner.width.saturating_sub(1) as usize;
        let row_of = |app: &App| {
            let conversation = conversation_lines(app, width);
            conversation.tool_headers[0].0 - conversation.offset(inner.height, app.scroll)
        };
        let before = row_of(&app);
        toggle_tool(&mut app, screen, ToolRow::Entry(10));
        assert!(app.entries[10].expanded);
        assert_eq!(row_of(&app), before);
    }

    #[test]
    fn a_call_shows_full_paths_and_what_it_writes() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "Done");
        app.entries[0].input = Some(serde_json::json!({
            "path": "/home/me/project/crates/cli/src/tui/ui/conversation.rs",
            "content": "fn main() {\n    run();\n}"
        }));
        let text = rendered(&app, 50, 20);
        // The path wraps in full instead of being cut.
        assert!(text.contains("path"), "{text}");
        assert!(text.contains("/home/me/project/crates"), "{text}");
        assert!(text.contains("versation.rs"), "{text}");
        assert!(
            text.contains("fn main() {"),
            "collapsed shows what is written"
        );
        assert!(!text.contains("result"));

        app.entries[0].set_expanded(true);
        let text = rendered(&app, 50, 20);
        assert!(text.contains("content"));
        assert!(text.contains("│ }"));
        assert!(text.contains("result"));
        assert!(text.contains("│ Done"));
    }

    #[test]
    fn long_preview_lines_are_cut_with_an_ellipsis() {
        let lines = output_preview(&format!("\n{}\nb\nc\nd", "x".repeat(50)), 10);
        let text: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        assert_eq!(text.len(), 3);
        assert!(text[0].ends_with("xxxxxxxxx…"), "{}", text[0]);
        assert!(text[2].ends_with('c'));
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
        assert!(text.contains("✦  git.status ▸"));
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

    #[test]
    fn a_run_of_calls_reads_as_one_row_until_opened() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.actions = vec![
            serde_json::json!("git.status"),
            serde_json::json!("git.diff"),
        ];
        app.push(EntryKind::User, "check the repo");
        app.push_tool("git.status", "clean");
        app.push_tool("git.diff", "");
        app.push_tool("web_search", "");
        let text = |app: &App| {
            conversation_lines(app, 60)
                .lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let closed = text(&app);
        assert!(closed.contains("Used 2 actions and 1 tool ▸"), "{closed}");
        assert!(!closed.contains("git.diff"), "{closed}");

        let screen = Rect::new(0, 0, 60, 24);
        toggle_tool(&mut app, screen, ToolRow::Group(1));
        let open = text(&app);
        assert!(open.contains("Used 2 actions and 1 tool ▾"), "{open}");
        assert!(
            open.contains("git.diff") && open.contains("web_search"),
            "{open}"
        );
        assert_eq!(app.latest_tool_row(), Some(ToolRow::Entry(1)));
    }

    #[test]
    fn working_shows_inline_and_queued_messages_follow_it() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "first");
        app.pending = true;
        app.queue.push_back("second".into());
        let text = conversation_lines(&app, 60)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("working"), "{text}");
        assert!(text.contains("second  queued"), "{text}");
    }
}

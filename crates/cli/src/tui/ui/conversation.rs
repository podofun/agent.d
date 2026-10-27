use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::super::app::{App, Entry, EntryKind, FileChange, ToolRow};
use super::super::commands;
use super::super::presentation;
use super::{
    ERROR, MUTED, PRIMARY, SECONDARY, SUCCESS, WARNING, highlight, markdown, palette, screen_rows,
    selection, spinner_frame,
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
                // Calls in an open group sit one gutter further in.
                let call_width = body_width.saturating_sub(GUTTER).max(1);
                for (offset, call) in calls.iter().enumerate() {
                    let block = call.lines(call_width, || entry_lines(call, call_width));
                    tool_headers.extend(
                        toggle_rows(call, &block)
                            .map(|row| (lines.len() + row, ToolRow::Entry(run.start + offset))),
                    );
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
            let block = entry.lines(body_width, || entry_lines(entry, body_width));
            tool_headers.extend(
                toggle_rows(entry, &block).map(|row| (lines.len() + row, ToolRow::Entry(index))),
            );
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

/// Rows of an entry's block that open or close it when clicked: a tool
/// row's header, and the first line of each changed file under it.
fn toggle_rows<'a>(
    entry: &'a Entry,
    block: &'a [Line<'static>],
) -> impl Iterator<Item = usize> + 'a {
    let expandable = entry.is_expandable_tool();
    block
        .iter()
        .enumerate()
        .filter(move |(row, line)| {
            expandable
                && (*row == 0
                    || line
                        .spans
                        .get(2)
                        .is_some_and(|span| span.content == CHANGE_MARK.trim_end()))
        })
        .map(|(row, _)| row)
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

/// Changed files listed on a collapsed row, and diffs shown on an expanded
/// one, before the rest are counted instead.
const COLLAPSED_FILES: usize = 8;
const EXPANDED_FILES: usize = 20;
/// Arguments that carry what a call wrote, left out of an expanded row that
/// shows the diff instead.
const SHOWN_BY_DIFF: [&str; 3] = ["content", "patch", "diff"];
/// Starts the line of each file a call changed.
const CHANGE_MARK: &str = "± ";

/// A call's row: its name, each short argument (paths, patterns, commands)
/// in full, the size of each long one, and the files the call changed.
/// Collapsed, a call that changed no files previews the first lines of its
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
    for (key, text) in &long {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{key}  {}", size_of(text)),
            Style::default().fg(MUTED),
        ));
    }
    let mut lines = with_gutter("✦", WARNING, markdown::wrap_spans(spans, width));
    let inner = width.saturating_sub(2);
    if !entry.expanded {
        if entry.changes.is_empty() {
            lines.extend(output_preview(&entry.body, inner, 3));
            return lines;
        }
        if entry.failed {
            lines.extend(output_preview(&entry.body, inner, 1));
        }
    }
    let shown = if entry.expanded {
        EXPANDED_FILES
    } else {
        COLLAPSED_FILES
    };
    for change in entry.changes.iter().take(shown) {
        lines.extend(change_summary(change, inner));
        if entry.expanded {
            lines.extend(diff_lines(change, inner));
        }
    }
    if let Some(more) = entry.changes.len().checked_sub(shown).filter(|n| *n > 0) {
        let files = if more == 1 { "file" } else { "files" };
        lines.push(indented(Line::styled(
            format!("  and {more} more {files}"),
            Style::default().fg(MUTED),
        )));
    }
    if !entry.expanded {
        return lines;
    }
    // The diff already shows what was written.
    let long: Fields = if entry.changes.is_empty() {
        long
    } else {
        long.into_iter()
            .filter(|(key, _)| !SHOWN_BY_DIFF.contains(&key.as_str()))
            .collect()
    };
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

/// A change's diff, one `│ ` line per diff line, cut to `width` and never
/// wrapped. Hunk headers are muted, the `+` and `-` markers colored, and
/// the code after them highlighted by the file's type. The `---` and `+++`
/// file headers are left out, since the summary line names the file. Lines
/// skip the highlight memo, since the row keeps its rendered lines.
fn diff_lines(change: &FileChange, width: usize) -> Vec<Line<'static>> {
    let Some(diff) = &change.diff else {
        return Vec::new();
    };
    let syntax = highlight::Syntax::find(highlight::language_for_path(&change.path));
    let muted = Style::default().fg(MUTED);
    let code = |text: &str| -> Vec<Span<'static>> {
        syntax
            .and_then(|syntax| syntax.line(text))
            .unwrap_or_else(|| vec![Span::raw(text.to_owned())])
    };
    let mut in_hunk = false;
    let mut lines = Vec::new();
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk && (line.starts_with("---") || line.starts_with("+++")) {
            continue;
        }
        let line = markdown::printable(line);
        let mut spans = match line.chars().next() {
            Some('+') if in_hunk => vec![Span::styled("+", Style::default().fg(SUCCESS))],
            Some('-') if in_hunk => vec![Span::styled("-", Style::default().fg(ERROR))],
            Some(' ') if in_hunk => vec![Span::raw(" ")],
            _ => {
                lines.push(diff_row(vec![Span::styled(line.into_owned(), muted)], width));
                continue;
            }
        };
        spans.extend(code(&line[1..]));
        lines.push(diff_row(spans, width));
    }
    lines
}

/// One diff line behind its `│ ` bar, cut to `width` with a `…` when it
/// does not fit.
fn diff_row(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut line = vec![Span::styled("│ ", Style::default().fg(MUTED))];
    if spans.iter().map(Span::width).sum::<usize>() > width {
        line.extend(markdown::cut_spans(spans, width.saturating_sub(1)));
        line.push(Span::styled("…", Style::default().fg(MUTED)));
    } else {
        line.extend(spans);
    }
    indented(Line::from(line))
}

/// "3 lines" for multi-line text, "250 chars" for a single long line.
fn size_of(text: &str) -> String {
    match text.lines().count() {
        0 | 1 => format!("{} chars", text.chars().count()),
        lines => format!("{lines} lines"),
    }
}

/// `± path  +12 -3`: one file a call changed, and how. Created and removed
/// files say so, and a change with no diff says why.
fn change_summary(change: &FileChange, width: usize) -> Vec<Line<'static>> {
    let muted = Style::default().fg(MUTED);
    let created = change.operation == "created";
    let removed = change.operation == "removed";
    let mut spans = vec![
        Span::styled(CHANGE_MARK, muted),
        Span::raw(change.path.clone()),
    ];
    let note = if change.binary {
        Some(if created {
            "new binary file"
        } else if removed {
            "removed"
        } else {
            "binary file changed"
        })
    } else if created {
        Some("new")
    } else if removed {
        Some("removed")
    } else {
        None
    };
    if let Some(note) = note {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(note, muted));
    }
    if !change.binary && change.diff.is_none() && !removed {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("too large to show", muted));
    } else if !change.binary && change.diff.is_some() {
        spans.push(Span::raw("  "));
        if !removed {
            spans.push(Span::styled(
                format!("+{}", change.added),
                Style::default().fg(SUCCESS),
            ));
        }
        if !created && !removed {
            spans.push(Span::raw(" "));
        }
        if !created {
            spans.push(Span::styled(
                format!("-{}", change.removed),
                Style::default().fg(ERROR),
            ));
        }
    }
    markdown::wrap_spans(spans, width)
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, Span::raw("  "));
            indented(line)
        })
        .collect()
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
fn output_preview(body: &str, width: usize, rows: usize) -> Vec<Line<'static>> {
    body.lines()
        .map(str::trim_end)
        .skip_while(|line| line.trim().is_empty())
        .take(rows)
        .map(|line| {
            let line = markdown::printable(line);
            let line = line.as_ref();
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
        assert!(text.contains("content  3 lines"), "{text}");
        assert!(
            !text.contains("fn main() {"),
            "collapsed rows do not preview long arguments"
        );
        assert!(!text.contains("result"));

        app.entries[0].set_expanded(true);
        let text = rendered(&app, 50, 20);
        assert!(text.contains("content"));
        assert!(text.contains("│ }"));
        assert!(text.contains("result"));
        assert!(text.contains("│ Done"));
    }

    fn change(path: &str, operation: &str, diff: Option<&str>) -> FileChange {
        FileChange {
            path: path.to_owned(),
            operation: operation.to_owned(),
            diff: diff.map(str::to_owned),
            binary: false,
            added: 2,
            removed: 1,
        }
    }

    fn plain_lines(app: &App, width: usize) -> Vec<String> {
        conversation_lines(app, width)
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect()
    }

    fn changed_call() -> App {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.patch", "Patched");
        app.entries[0].input = Some(json!({ "path": "src/a.rs" }));
        app.entries[0].set_changes(vec![change(
            "src/a.rs",
            "modified",
            Some("--- src/a.rs\n+++ src/a.rs\n@@ -1 +1,2 @@\n-a\n+b\n+c\n"),
        )]);
        app
    }

    #[test]
    fn collapsed_rows_summarize_changes() {
        let app = changed_call();
        let lines = plain_lines(&app, 60);
        assert!(
            lines.iter().any(|line| line.contains("± src/a.rs  +2 -1")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|line| line.contains("+b")), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("Patched")),
            "the change replaces the output preview: {lines:?}"
        );
        let summary = conversation_lines(&app, 60)
            .lines
            .into_iter()
            .find(|line| line.to_string().contains("± src/a.rs"))
            .unwrap();
        let fg = |text: &str| {
            summary
                .spans
                .iter()
                .find(|span| span.content == text)
                .and_then(|span| span.style.fg)
        };
        assert_eq!(fg("+2"), Some(SUCCESS));
        assert_eq!(fg("-1"), Some(ERROR));
    }

    #[test]
    fn created_and_removed_files_say_so() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("shell", "");
        app.entries[0].set_changes(vec![
            change("new.rs", "created", Some("@@ -0,0 +1,2 @@\n+a\n+b\n")),
            change("old.rs", "removed", Some("@@ -1 +0,0 @@\n-a\n")),
        ]);
        let lines = plain_lines(&app, 60);
        assert!(
            lines.iter().any(|line| line.contains("± new.rs  new  +2")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("± old.rs  removed  -1")),
            "{lines:?}"
        );
    }

    #[test]
    fn long_arguments_are_summarized_not_previewed() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "");
        app.entries[0].input = Some(json!({ "path": "a.rs", "content": "x\ny\nz" }));
        app.entries[0].set_expanded(false);
        let lines = plain_lines(&app, 60);
        assert!(lines[0].contains("content  3 lines"), "{lines:?}");
        assert!(!lines.iter().any(|line| line.trim() == "x"), "{lines:?}");

        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "");
        app.entries[0].input = Some(json!({ "content": "y".repeat(250) }));
        let lines = plain_lines(&app, 60);
        assert!(lines[0].contains("content  250 chars"), "{lines:?}");
    }

    #[test]
    fn expanded_rows_show_the_diff() {
        let mut app = changed_call();
        app.entries[0].set_expanded(true);
        let lines = plain_lines(&app, 60);
        let position = |needle: &str| {
            lines
                .iter()
                .position(|line| line.trim_end().ends_with(needle))
                .unwrap_or_else(|| panic!("no {needle:?} in {lines:?}"))
        };
        let order = [
            position("± src/a.rs  +2 -1"),
            position("│ @@ -1 +1,2 @@"),
            position("│ -a"),
            position("│ +b"),
            position("│ +c"),
            position("│ Patched"),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{lines:?}");
        assert!(
            !lines.iter().any(|line| line.contains("+++")),
            "file headers are left out: {lines:?}"
        );
    }

    fn diff_line(app: &App, width: usize, needle: &str) -> Line<'static> {
        conversation_lines(app, width)
            .lines
            .into_iter()
            .find(|line| line.to_string().trim_end().ends_with(needle))
            .unwrap_or_else(|| panic!("no {needle:?}"))
    }

    fn after_bar(line: &Line<'static>) -> Vec<Span<'static>> {
        let bar = line
            .spans
            .iter()
            .position(|span| span.content == "│ ")
            .unwrap();
        line.spans[bar + 1..].to_vec()
    }

    #[test]
    fn diff_markers_are_colored() {
        let mut app = changed_call();
        app.entries[0].set_expanded(true);
        let added = after_bar(&diff_line(&app, 60, "│ +b"));
        assert_eq!(added[0].content, "+");
        assert_eq!(added[0].style.fg, Some(SUCCESS));
        let removed = after_bar(&diff_line(&app, 60, "│ -a"));
        assert_eq!(removed[0].content, "-");
        assert_eq!(removed[0].style.fg, Some(ERROR));
        let hunk = after_bar(&diff_line(&app, 60, "│ @@ -1 +1,2 @@"));
        assert!(hunk.iter().all(|span| span.style.fg == Some(MUTED)));
    }

    #[test]
    fn diff_code_is_highlighted_by_file_type() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "");
        app.entries[0].set_changes(vec![change(
            "x.rs",
            "modified",
            Some("@@ -1 +1 @@\n-fn e() {}\n+fn f() {}\n"),
        )]);
        app.entries[0].set_expanded(true);
        let spans = after_bar(&diff_line(&app, 60, "│ +fn f() {}"));
        assert!(
            spans[1..]
                .iter()
                .any(|span| span.style.fg.is_some_and(|fg| fg != SUCCESS)),
            "{spans:?}"
        );
    }

    #[test]
    fn diff_lines_are_cut_not_wrapped() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "");
        let long = format!("@@ -0,0 +1 @@\n+let s = \"{}\";\n", "日本語".repeat(10));
        app.entries[0].set_changes(vec![change("x.rs", "created", Some(&long))]);
        app.entries[0].set_expanded(true);
        let lines = conversation_lines(&app, 30).lines;
        let cut = lines
            .iter()
            .find(|line| line.to_string().contains("│ +let s"))
            .unwrap();
        assert!(cut.width() <= 30, "{}", cut.width());
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.to_string().contains("日本"))
                .count(),
            1
        );
    }

    fn expanded_diff(path: &str, diff: &str) -> App {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "");
        app.entries[0].set_changes(vec![change(path, "modified", Some(diff))]);
        app.entries[0].set_expanded(true);
        app
    }

    #[test]
    fn cut_diff_lines_end_with_an_ellipsis() {
        let app = expanded_diff("x.txt", &format!("@@ -1 +1 @@\n+{}\n", "x".repeat(80)));
        let line = diff_line(&app, 30, "…");
        assert_eq!(line.width(), 30, "{line}");
        assert!(line.to_string().contains("│ +xxx"), "{line}");
        let short = diff_line(&app, 30, "@@ -1 +1 @@");
        assert!(!short.to_string().contains('…'));
    }

    #[test]
    fn collapsed_rows_cap_the_file_list() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("shell", "");
        app.entries[0].set_changes(
            (0..11)
                .map(|n| {
                    change(
                        &format!("f{n}.rs"),
                        "modified",
                        Some("@@ -1 +1 @@\n-a\n+b\n"),
                    )
                })
                .collect(),
        );
        let lines = plain_lines(&app, 60);
        assert_eq!(lines.iter().filter(|line| line.contains('±')).count(), 8);
        let more = lines
            .iter()
            .position(|line| line.trim_end().ends_with("and 3 more files"))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(lines[more - 1].contains("± f7.rs"), "{lines:?}");
    }

    #[test]
    fn expanded_rows_cap_the_diffs() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("shell", "");
        app.entries[0].set_changes(
            (0..21)
                .map(|n| {
                    change(
                        &format!("f{n}.rs"),
                        "modified",
                        Some("@@ -1 +1 @@\n-a\n+b\n"),
                    )
                })
                .collect(),
        );
        app.entries[0].set_expanded(true);
        let lines = plain_lines(&app, 60);
        assert_eq!(lines.iter().filter(|line| line.contains('±')).count(), 20);
        assert_eq!(
            lines.iter().filter(|line| line.ends_with("│ +b")).count(),
            20
        );
        assert!(
            lines
                .iter()
                .any(|line| line.trim_end().ends_with("and 1 more file")),
            "{lines:?}"
        );
    }

    #[test]
    fn open_groups_fit_their_rows_in_the_width() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        let path = format!("crates/{}/main.rs", "deep".repeat(15));
        app.push_tool("fs.write", "");
        app.entries[0].input = Some(json!({ "path": path }));
        app.entries[0].set_changes(vec![change(
            &path,
            "modified",
            Some(&format!("@@ -1 +1 @@\n-a\n+{}\n", "y".repeat(80))),
        )]);
        app.entries[0].set_expanded(true);
        app.push_tool("git.status", "clean");
        app.entries[0].group_open = true;
        let conversation = conversation_lines(&app, 40);
        for line in &conversation.lines {
            assert!(line.width() <= 40, "{} wide: {line}", line.width());
        }
        assert!(
            conversation
                .lines
                .iter()
                .any(|line| line.to_string().contains("crates/deepdeep"))
        );
    }

    #[test]
    fn tabs_in_diffs_become_spaces() {
        let app = expanded_diff("x.py", "@@ -1 +1 @@\n-\tx = 0\n+\tx = 1\n");
        let lines = plain_lines(&app, 60);
        assert!(
            lines.iter().any(|line| line.ends_with("│ +    x = 1")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.ends_with("│ -    x = 0")),
            "{lines:?}"
        );
    }

    #[test]
    fn diff_lines_leave_reply_code_in_the_memo() {
        let diff: String = std::iter::once("@@ -0,0 +1,100 @@".to_owned())
            .chain((0..100).map(|n| format!("+let diff_memo_{n} = {n};")))
            .collect::<Vec<_>>()
            .join("\n");
        let app = expanded_diff("x.rs", &diff);
        // Each memo entry the diff added could push out a block of a reply.
        let before = highlight::MISSES.with(|misses| misses.get());
        assert!(
            plain_lines(&app, 60)
                .iter()
                .any(|line| line.contains("diff_memo_99"))
        );
        assert_eq!(highlight::MISSES.with(|misses| misses.get()), before);
    }

    #[test]
    fn failed_calls_keep_their_output_above_the_files() {
        let result = |output: Value, error: Option<&str>| {
            let mut app = App::new("http://127.0.0.1:7777", 1000, None);
            app.apply(super::super::super::app::Update::Delta(json!({
                "type": "tool_result", "name": "shell", "output": output, "error": error,
                "changes": [{ "path": "a.txt", "operation": "created", "diff": "@@ -0,0 +1 @@\n+a\n", "added": 1 }]
            })));
            plain_lines(&app, 60)
        };
        let failed = result(json!({ "exit_code": 2, "stderr": "boom" }), None);
        assert!(!failed[1].contains('±'), "{failed:?}");
        assert!(failed[2].contains("± a.txt"), "{failed:?}");
        let failed = result(
            json!({ "exit_code": 2, "stderr": "boom" })
                .to_string()
                .into(),
            None,
        );
        assert!(failed[2].contains("± a.txt"), "{failed:?}");
        let errored = result(Value::Null, Some("denied"));
        assert!(errored[1].contains("failed: denied"), "{errored:?}");
        assert!(errored[2].contains("± a.txt"), "{errored:?}");
        let passed = result(json!({ "exit_code": 0, "stdout": "fine" }), None);
        assert!(passed[1].contains("± a.txt"), "{passed:?}");
    }

    #[test]
    fn expanded_changed_calls_do_not_repeat_content() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "Done");
        app.entries[0].input = Some(json!({ "path": "a.txt", "content": "one\ntwo\nthree" }));
        app.entries[0].set_changes(vec![change(
            "a.txt",
            "created",
            Some("@@ -0,0 +1,3 @@\n+one\n+two\n+three\n"),
        )]);
        app.entries[0].set_expanded(true);
        let lines = plain_lines(&app, 60);
        assert!(lines[0].contains("content  3 lines"), "{lines:?}");
        assert!(
            lines.iter().any(|line| line.ends_with("│ +two")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.ends_with("│ two")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.trim() == "result"),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.ends_with("│ Done")),
            "{lines:?}"
        );
    }

    #[test]
    fn summary_lines_toggle_the_row() {
        let mut app = changed_call();
        for expanded in [false, true] {
            app.entries[0].set_expanded(expanded);
            let conversation = conversation_lines(&app, 60);
            let summary = conversation
                .lines
                .iter()
                .position(|line| line.to_string().contains("± src/a.rs"))
                .unwrap();
            assert!(
                conversation
                    .tool_headers
                    .contains(&(summary, ToolRow::Entry(0))),
                "{:?}",
                conversation.tool_headers
            );
            assert!(
                !conversation
                    .tool_headers
                    .iter()
                    .any(|(line, _)| conversation.lines[*line].to_string().contains("│ +b"))
            );
        }
    }

    #[test]
    fn crlf_diffs_render_without_carriage_returns() {
        let app = expanded_diff("a.txt", "@@ -1 +1 @@\r\n-a\r\n+b\r\n");
        let lines = plain_lines(&app, 60);
        assert!(lines.iter().any(|line| line.ends_with("│ +b")), "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains('\r')), "{lines:?}");
    }

    #[test]
    fn diff_notes_are_muted() {
        let app = expanded_diff(
            "a.txt",
            "@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n... 5 more lines\n",
        );
        for note in ["\\ No newline at end of file", "... 5 more lines"] {
            let spans = after_bar(&diff_line(&app, 60, note));
            assert!(
                spans.iter().all(|span| span.style.fg == Some(MUTED)),
                "{spans:?}"
            );
        }
    }

    #[test]
    fn removed_lines_that_look_like_file_headers_stay() {
        let app = expanded_diff("a.txt", "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n--- x\n+++ y\n");
        let lines = plain_lines(&app, 60);
        assert!(
            !lines.iter().any(|line| line.ends_with("--- a.txt")),
            "{lines:?}"
        );
        let removed = after_bar(&diff_line(&app, 60, "│ --- x"));
        assert_eq!(removed[0].content, "-");
        assert_eq!(removed[0].style.fg, Some(ERROR));
        assert!(
            lines.iter().any(|line| line.ends_with("│ +++ y")),
            "{lines:?}"
        );
    }

    #[test]
    fn binary_and_missing_diffs_say_so() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("shell", "");
        let mut binary = change("logo.png", "modified", None);
        binary.binary = true;
        app.entries[0].set_changes(vec![binary, change("big.log", "modified", None)]);
        let lines = plain_lines(&app, 60);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("± logo.png  binary file changed")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("± big.log  too large to show")),
            "{lines:?}"
        );
    }

    #[test]
    fn older_results_without_changes_render_as_before() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.apply(super::super::super::app::Update::Delta(json!({
            "type": "tool_result", "name": "git.status",
            "output": "yes it worked\ndiff:\nmodified: README.md"
        })));
        assert!(app.entries[0].changes.is_empty());
        let lines = plain_lines(&app, 60);
        assert!(lines[0].contains("✦  git.status ▸"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("modified: README.md"))
        );
        assert!(!lines.iter().any(|line| line.contains("│ modified")));
        assert!(!lines.iter().any(|line| line.contains('±')));
        app.entries[0].set_expanded(true);
        let lines = plain_lines(&app, 60);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("│ modified: README.md"))
        );
    }

    #[test]
    fn long_preview_lines_are_cut_with_an_ellipsis() {
        let lines = output_preview(&format!("\n{}\nb\nc\nd", "x".repeat(50)), 10, 3);
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

    /// Every cell the conversation draws, row by row.
    fn screen(app: &App, width: u16, height: u16) -> (Vec<Vec<String>>, ratatui::buffer::Buffer) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::super::draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let cells = (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_owned()).collect())
            .collect();
        (cells, buffer)
    }

    /// A read whose output has `cat -n` tabs and a carriage return, then an
    /// expanded edit whose text has an emoji and an em dash.
    fn tabbed_and_wide() -> App {
        let file = "#!/usr/bin/env bash\n# Throwaway demo script #2 \u{2014} prints a greeting, then exits.\nset -euo pipefail\necho \"Hello from a throwaway script! \u{1F44B}\"\necho \"This file is temporary \u{2014} run it once.\"\necho \"Today is $(date -u +%A).\"\n";
        let numbered: String = file
            .lines()
            .enumerate()
            .map(|(row, line)| format!("{:>6}\t{line}\r\n", row + 1))
            .collect();
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.connected = true;
        app.push_tool("fs.read", numbered.clone());
        app.entries[0].input = Some(json!({ "path": "demo.sh" }));
        app.entries[0].set_expanded(true);
        app.push_tool("edit.replace", "ok");
        let old = file.replace("Hello", "Hi");
        app.entries[1].input = Some(json!({ "path": "demo.sh", "old": old, "new": file }));
        let diff = format!(
            "@@ -4 +4 @@\n-echo \"Hi from a throwaway script! \u{1F44B}\"\n+echo \"Hello from a\tthrowaway script! \u{1F44B}\"\n{}",
            numbered.lines().map(|line| format!(" {line}\n")).collect::<String>()
        );
        app.entries[1].set_changes(vec![change("demo.sh", "modified", Some(&diff))]);
        app.entries[1].set_expanded(true);
        // Two calls in a row read as one group row until it is opened.
        app.entries[0].group_open = true;
        app
    }

    #[test]
    fn control_characters_never_reach_the_screen() {
        let app = tabbed_and_wide();
        for width in [40, 61, 80] {
            let (cells, _) = screen(&app, width, 60);
            for (y, row) in cells.iter().enumerate() {
                assert!(
                    row.iter().all(|cell| !cell.contains(char::is_control)),
                    "row {y} at width {width}: {row:?}"
                );
            }
            let text_width = conversation_inner(Rect::new(0, 0, width, 60)).width as usize - 1;
            for line in conversation_lines(&app, text_width).lines {
                assert!(line.width() <= text_width, "{} > {text_width}: {line}", line.width());
                assert!(!line.to_string().contains(char::is_control), "{line:?}");
            }
        }
        let preview = output_preview("     1\tfirst\r", 30, 1);
        assert_eq!(preview[0].to_string().trim(), "1    first");
    }

    #[test]
    fn redraws_leave_no_stale_cells() {
        let mut app = tabbed_and_wide();
        let (width, height) = (61, 24);
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let screen_rect = Rect::new(0, 0, width, height);
        let steps = max_scroll(&app, screen_rect);
        assert!(steps > 4, "the conversation should scroll: {steps}");
        for scroll in (0..=steps).chain((0..=steps).rev()) {
            app.scroll = scroll;
            terminal.draw(|frame| super::super::draw(frame, &app)).unwrap();
            // The terminal kept only the cells each draw changed; they must
            // add up to what a fresh draw of the same state shows.
            let (_, fresh) = screen(&app, width, height);
            let kept = terminal.backend().buffer();
            for y in 0..height {
                let last = &kept[(width - 1, y)];
                assert!(last.symbol().width() <= 1, "wide cell on the edge at row {y}");
                for x in 0..width {
                    // A terminal draws a wide character over the next cell
                    // too, which the test backend leaves as it was.
                    if x > 0 && fresh[(x - 1, y)].symbol().width() > 1 {
                        continue;
                    }
                    assert_eq!(
                        kept[(x, y)].symbol(),
                        fresh[(x, y)].symbol(),
                        "stale cell at ({x}, {y}) after scrolling to {scroll}"
                    );
                }
            }
        }
        app.entries[1].set_expanded(false);
        terminal.draw(|frame| super::super::draw(frame, &app)).unwrap();
        let (fresh, _) = screen(&app, width, height);
        let kept = terminal.backend().buffer();
        for (y, row) in fresh.iter().enumerate() {
            for (x, cell) in row.iter().enumerate() {
                if x > 0 && row[x - 1].width() > 1 {
                    continue;
                }
                assert_eq!(kept[(x as u16, y as u16)].symbol(), cell, "({x}, {y})");
            }
        }
    }
}

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
    if let Some(selection) = app.selection.as_ref().filter(|s| !s.is_click()) {
        selection::highlight(&mut conversation.lines, &conversation.selectable, selection);
    }
    // The whole row is clickable, but only its header reacts to the mouse:
    // what the row holds keeps its own colors.
    if let Some((_, row)) = app.hover
        && let Some(&(header, _)) = conversation
            .tool_headers
            .iter()
            .find(|(_, target)| *target == row)
        && let Some(line) = conversation.lines.get_mut(header)
    {
        hovered(line);
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
/// The marker of a row that opens, and of one that is open.
const CLOSED: &str = "▶";
const OPEN: &str = "▼";
/// A tool row that is open or under the mouse, in a soft white.
const ACTIVE: Color = Color::Rgb(205, 209, 216);

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

/// A line inside an open group, behind the group's nesting line.
fn nested(mut line: Line<'static>) -> Line<'static> {
    line.spans
        .insert(0, Span::styled("│  ", Style::default().fg(MUTED)));
    line
}

/// A call inside a group: a small muted dot instead of the call's own
/// marker, so the group's marker stands for all of them.
fn grouped(mut line: Line<'static>) -> Line<'static> {
    if let Some(first) = line.spans.first_mut()
        && first.content.starts_with('✦')
    {
        *first = Span::styled("•  ", Style::default().fg(MUTED));
    }
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
                    lines.extend(
                        block
                            .into_iter()
                            .enumerate()
                            .map(|(row, line)| nested(if row == 0 { grouped(line) } else { line })),
                    );
                }
            }
        } else {
            if entry.hidden() {
                index = run.end;
                continue;
            }
            let attached = |nodes: &[agentd_ui::NodeId]| -> Vec<Line<'static>> {
                nodes
                    .iter()
                    .flat_map(|node| super::element_lines(&app.script.tree, *node, body_width))
                    .map(indented)
                    .collect()
            };
            let (before, after) = entry
                .decoration
                .as_ref()
                .map(|d| (attached(&d.before), attached(&d.after)))
                .unwrap_or_default();
            selectable.extend(before.iter().map(|_| 0..0));
            lines.extend(before);
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
            selectable.extend(after.iter().map(|_| 0..0));
            lines.extend(after);
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

/// Rows of an entry's block that open or close it when clicked: every row
/// of a tool call, so the whole call acts as one control. Dragging over it
/// still selects its text.
fn toggle_rows<'a>(
    entry: &'a Entry,
    block: &'a [Line<'static>],
) -> impl Iterator<Item = usize> + 'a {
    let rows = if entry.is_expandable_tool() {
        block.len()
    } else {
        0
    };
    0..rows
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
    let marker = if open { OPEN } else { CLOSED };
    // An open row keeps the plain color the mouse gives it.
    let style = if open {
        Style::default().fg(ACTIVE)
    } else {
        Style::default().fg(MUTED)
    };
    let title: String = label
        .graphemes(true)
        .take(width.saturating_sub(2))
        .collect();
    Line::from(vec![
        gutter("✦", WARNING),
        Span::styled(format!("{title} "), style),
        Span::styled(marker, style),
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
    let lines = kind_lines(entry, width);
    match entry.decoration.as_ref().filter(|d| !d.style.is_empty()) {
        Some(decoration) => {
            let style = super::script::style_of(&decoration.style);
            lines
                .into_iter()
                .map(|line| line.patch_style(style))
                .collect()
        }
        None => lines,
    }
}

fn kind_lines(entry: &Entry, width: usize) -> Vec<Line<'static>> {
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
        (true, true) => " ▼",
        (true, false) => " ▶",
    };
    // An open row keeps the plain color the mouse gives it.
    let title = if entry.expanded {
        Style::default().fg(ACTIVE)
    } else {
        Style::default().fg(MUTED)
    };
    let mut spans = vec![
        Span::styled(entry.text.clone(), title),
        Span::styled(marker, title),
    ];
    for (key, value) in &short {
        let key = if key == "path" { "file" } else { key.as_str() };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(format!("{key} "), Style::default().fg(MUTED)));
        spans.push(Span::raw(value.clone()));
    }
    // A chat saved before calls kept their changes still shows an edit or a
    // write as a diff, worked out from the text the call was given.
    let derived;
    let changes: &[FileChange] = if entry.changes.is_empty() {
        derived = derived_change(entry);
        derived.as_slice()
    } else {
        &entry.changes
    };
    let mut header = markdown::wrap_spans(spans, width);
    if entry.expanded {
        header.iter_mut().for_each(active);
    }
    let mut lines = with_gutter("✦", WARNING, header);
    if !entry.expanded
        && let Some(summary) = entry.summary()
    {
        lines.push(indented(Line::styled(
            format!("  {}", agentd_ui::sanitize(summary)),
            Style::default().fg(MUTED),
        )));
        return lines;
    }
    let inner = width.saturating_sub(2);
    if !entry.expanded {
        if changes.is_empty() {
            lines.extend(output_preview(preview_text(entry), inner, 3));
            return lines;
        }
        if entry.failed {
            lines.extend(output_preview(preview_text(entry), inner, 1));
        }
    }
    let shown = if entry.expanded {
        EXPANDED_FILES
    } else {
        COLLAPSED_FILES
    };
    for change in changes.iter().take(shown) {
        lines.extend(change_summary(change, inner));
        if entry.expanded {
            lines.extend(diff_lines(change, inner));
        }
    }
    if let Some(more) = changes.len().checked_sub(shown).filter(|n| *n > 0) {
        let files = if more == 1 { "file" } else { "files" };
        lines.push(indented(Line::styled(
            format!("  and {more} more {files}"),
            Style::default().fg(MUTED),
        )));
    }
    if !entry.expanded {
        return lines;
    }
    // A call that changed files shows what it did as their diffs: the text
    // it was given and the receipt it returned would only repeat them.
    if !changes.is_empty() && !entry.failed {
        return lines;
    }
    let long = if changes.is_empty() { long } else { Vec::new() };
    let input = entry.input.as_ref().and_then(Value::as_object);
    for (key, text) in &long {
        match input
            .and_then(|fields| fields.get(key))
            .and_then(|value| checklist(value))
        {
            Some(items) => lines.extend(items),
            None => block(&mut lines, inner, key, text, MUTED),
        }
    }
    match entry.output.as_ref() {
        Some(output) if run_output(output).is_some() => {
            lines.extend(run_lines(output, inner));
        }
        Some(Value::Object(fields)) if !entry.failed => {
            lines.extend(field_lines(fields, input, inner));
        }
        _ if !entry.body.is_empty() => {
            let label = if long.is_empty() { "" } else { "result" };
            block(&mut lines, inner, label, &entry.body, WARNING);
        }
        _ => {}
    }
    lines
}

/// The change an edit (`path`, `old`, `new`) or a write (`path`,
/// `content`) made, worked out from its arguments, for calls saved without
/// their changes. Nothing for any other call, or one that failed.
fn derived_change(entry: &Entry) -> Vec<FileChange> {
    let Some(Value::Object(input)) = entry.input.as_ref() else {
        return Vec::new();
    };
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let Some(path) = text("path").filter(|_| !entry.failed) else {
        return Vec::new();
    };
    let (old, new) = match (text("old"), text("new"), text("content")) {
        (Some(old), Some(new), _) => (old, new),
        (None, None, Some(content)) if entry.text.ends_with("write") => ("", content),
        _ => return Vec::new(),
    };
    let mut diff = String::from("@@ @@\n");
    for line in old.lines() {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in new.lines() {
        diff.push_str(&format!("+{line}\n"));
    }
    vec![FileChange {
        path: path.to_owned(),
        operation: "modified".to_owned(),
        diff: Some(diff),
        binary: false,
        added: new.lines().count(),
        removed: old.lines().count(),
    }]
}

/// A labelled block of text behind a `│ ` bar in `bar`'s color.
fn block(lines: &mut Vec<Line<'static>>, width: usize, label: &str, text: &str, bar: Color) {
    labelled(lines, label, Style::default().fg(MUTED));
    bar_lines(lines, width, text, bar);
}

fn labelled(lines: &mut Vec<Line<'static>>, label: &str, style: Style) {
    if !label.is_empty() {
        lines.push(indented(Line::from(vec![
            Span::raw("  "),
            Span::styled(label.to_owned(), style),
        ])));
    }
}

fn bar_lines(lines: &mut Vec<Line<'static>>, width: usize, text: &str, bar: Color) {
    for mut part in markdown::wrap_plain(text, width) {
        part.spans
            .insert(0, Span::styled("│ ", Style::default().fg(bar)));
        lines.push(indented(part));
    }
}

/// The parts of a program run's output: what it printed, what it printed
/// as errors, how it exited, and whether it was stopped or cut short.
struct RunOutput<'a> {
    stdout: &'a str,
    stderr: &'a str,
    exit_code: Option<i64>,
    timed_out: bool,
    truncated: bool,
}

/// `output` read as a program run, when it has the shape of one: an
/// `exit_code`, or `stdout` or `stderr` text, and no other fields.
fn run_output(output: &Value) -> Option<RunOutput<'_>> {
    const FIELDS: [&str; 5] = ["stdout", "stderr", "exit_code", "timed_out", "truncated"];
    let fields = output.as_object()?;
    if !fields.keys().all(|key| FIELDS.contains(&key.as_str()))
        || !(fields.contains_key("exit_code")
            || fields.contains_key("stdout")
            || fields.contains_key("stderr"))
    {
        return None;
    }
    let text = |key: &str| match fields.get(key) {
        None | Some(Value::Null) => Some(""),
        Some(value) => value.as_str(),
    };
    let flag = |key: &str| match fields.get(key) {
        None | Some(Value::Null) => Some(false),
        Some(value) => value.as_bool(),
    };
    Some(RunOutput {
        stdout: text("stdout")?,
        stderr: text("stderr")?,
        exit_code: match fields.get("exit_code") {
            None | Some(Value::Null) => None,
            Some(code) => Some(code.as_i64()?),
        },
        timed_out: flag("timed_out")?,
        truncated: flag("truncated")?,
    })
}

/// A program run: anything wrong with it first, in red, then what it
/// printed. Empty errors, a zero exit code and unset flags are left out.
fn run_lines(output: &Value, width: usize) -> Vec<Line<'static>> {
    let Some(run) = run_output(output) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let error = Style::default().fg(ERROR);
    let mut notes = Vec::new();
    if let Some(code) = run.exit_code.filter(|code| *code != 0) {
        notes.push(format!("exit code {code}"));
    }
    if run.timed_out {
        notes.push("stopped: ran too long".to_owned());
    }
    if run.truncated {
        notes.push("output cut".to_owned());
    }
    for note in notes {
        labelled(&mut lines, &note, error);
    }
    if !run.stderr.trim().is_empty() {
        labelled(&mut lines, "errors", error);
        bar_lines(&mut lines, width, run.stderr.trim_end(), ERROR);
    }
    if !run.stdout.trim().is_empty() {
        labelled(&mut lines, "output", Style::default().fg(MUTED));
        bar_lines(&mut lines, width, run.stdout.trim_end(), MUTED);
    }
    lines
}

/// Other object results, one field per line: the label muted, the value
/// plain, and a value that does not fit on one line set below its label.
/// A `revision` is bookkeeping for undo, and a field that repeats one of
/// the call's own arguments (its path, say) is already on the header, so
/// both are left out.
fn field_lines(
    fields: &serde_json::Map<String, Value>,
    input: Option<&serde_json::Map<String, Value>>,
    width: usize,
) -> Vec<Line<'static>> {
    let label = Style::default().fg(MUTED);
    let mut lines = Vec::new();
    for (key, value) in fields {
        let repeats = input.is_some_and(|input| {
            input.get(key) == Some(value)
                || value
                    .as_str()
                    .is_some_and(|text| input.values().any(|v| v.as_str() == Some(text)))
        });
        if key == "revision" || repeats {
            continue;
        }
        if let Some(items) = checklist(value) {
            lines.extend(items);
            continue;
        }
        let name = presentation::field_label(key);
        let inline = match value {
            Value::Null => continue,
            Value::String(text) if !text.contains('\n') => Some(text.clone()),
            Value::Number(_) | Value::Bool(_) => Some(presentation::value(value)),
            _ => None,
        };
        match inline {
            Some(text) => {
                let spans = vec![Span::styled(format!("{name}  "), label), Span::raw(text)];
                for mut line in markdown::wrap_spans(spans, width) {
                    line.spans
                        .insert(0, Span::styled("│ ", Style::default().fg(WARNING)));
                    lines.push(indented(line));
                }
            }
            None => {
                lines.push(indented(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(WARNING)),
                    Span::styled(name, label),
                ])));
                let nested = presentation::value(value);
                for mut line in markdown::wrap_plain(&nested, width.saturating_sub(2)) {
                    line.spans.insert(0, Span::raw("  "));
                    line.spans
                        .insert(0, Span::styled("│ ", Style::default().fg(WARNING)));
                    lines.push(indented(line));
                }
            }
        }
    }
    lines
}

/// A list of `{ text, status }` items, such as an agent's todo list, as a
/// checklist: done items ticked and muted, the current one marked.
fn checklist(value: &Value) -> Option<Vec<Line<'static>>> {
    let items = value.as_array().filter(|items| !items.is_empty())?;
    items
        .iter()
        .map(|item| {
            let text = item.get("text")?.as_str()?;
            let status = item.get("status")?.as_str()?;
            let (mark, mark_style, text_style) = match status {
                "done" | "completed" => (
                    "✓",
                    Style::default().fg(SUCCESS),
                    Style::default().fg(MUTED),
                ),
                "in_progress" => ("●", Style::default().fg(WARNING), Style::default()),
                _ => ("○", Style::default().fg(MUTED), Style::default()),
            };
            Some(indented(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{mark} "), mark_style),
                Span::styled(markdown::printable(text).into_owned(), text_style),
            ])))
        })
        .collect()
}

/// A line's text as runs, each flagged when it changed.
type Marked = Vec<(String, bool)>;

/// Removed and added lines sit on a red or green background across the
/// whole row, and the words that changed on a stronger one.
const REMOVED_LINE: Color = Color::Indexed(52);
const REMOVED_WORD: Color = Color::Indexed(88);
const ADDED_LINE: Color = Color::Indexed(22);
const ADDED_WORD: Color = Color::Indexed(28);

/// A change's diff, one `│ ` line per diff line, cut to `width` and never
/// wrapped. Removed lines are red and added lines green, and when removed
/// lines are followed by added ones, each pair marks the words that changed.
/// Context lines are highlighted by the file's type. The first hunk header
/// is left out and later ones become a muted `⋯`, as are the `---` and
/// `+++` file headers, since the summary line names the file. Lines skip
/// the highlight memo, since the row keeps its rendered lines.
fn diff_lines(change: &FileChange, width: usize) -> Vec<Line<'static>> {
    let Some(diff) = &change.diff else {
        return Vec::new();
    };
    let syntax = highlight::Syntax::find(highlight::language_for_path(&change.path));
    let muted = Style::default().fg(MUTED);
    let mut hunks = 0;
    let mut lines = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    let flush =
        |lines: &mut Vec<Line<'static>>, removed: &mut Vec<String>, added: &mut Vec<String>| {
            let pairs = removed.len().min(added.len());
            let marks: Vec<(Marked, Marked)> = (0..pairs)
                .map(|index| word_diff(&removed[index], &added[index]))
                .collect();
            for (index, text) in removed.drain(..).enumerate() {
                let parts = marks
                    .get(index)
                    .map_or_else(|| vec![(text.clone(), false)], |pair| pair.0.clone());
                lines.push(filled_row(
                    changed_spans('-', REMOVED_LINE, REMOVED_WORD, parts),
                    width,
                    REMOVED_LINE,
                ));
            }
            for (index, text) in added.drain(..).enumerate() {
                let parts = marks
                    .get(index)
                    .map_or_else(|| vec![(text.clone(), false)], |pair| pair.1.clone());
                lines.push(filled_row(
                    changed_spans('+', ADDED_LINE, ADDED_WORD, parts),
                    width,
                    ADDED_LINE,
                ));
            }
        };
    for line in diff.lines() {
        let in_hunk = hunks > 0;
        if line.starts_with("@@") {
            flush(&mut lines, &mut removed, &mut added);
            if hunks > 0 {
                lines.push(diff_row(vec![Span::styled("⋯", muted)], width));
            }
            hunks += 1;
            continue;
        }
        if !in_hunk && (line.starts_with("---") || line.starts_with("+++")) {
            continue;
        }
        let line = markdown::printable(line);
        match line.chars().next() {
            Some('-') if in_hunk => {
                if !added.is_empty() {
                    flush(&mut lines, &mut removed, &mut added);
                }
                removed.push(line[1..].to_owned());
            }
            Some('+') if in_hunk => added.push(line[1..].to_owned()),
            Some(' ') if in_hunk => {
                flush(&mut lines, &mut removed, &mut added);
                let code = &line[1..];
                let mut spans = vec![Span::raw(" ")];
                spans.extend(
                    syntax
                        .and_then(|syntax| syntax.line(code))
                        .unwrap_or_else(|| vec![Span::raw(code.to_owned())]),
                );
                lines.push(diff_row(spans, width));
            }
            _ => {
                flush(&mut lines, &mut removed, &mut added);
                lines.push(diff_row(
                    vec![Span::styled(line.into_owned(), muted)],
                    width,
                ));
            }
        }
    }
    flush(&mut lines, &mut removed, &mut added);
    lines
}

/// One removed or added line: its marker and text on the `line`
/// background, and the parts marked as changed on the `word` one.
fn changed_spans(marker: char, line: Color, word: Color, parts: Marked) -> Vec<Span<'static>> {
    let plain = Style::default().bg(line);
    let mut spans = vec![Span::styled(marker.to_string(), plain)];
    spans.extend(
        parts.into_iter().map(|(text, changed)| {
            Span::styled(text, if changed { plain.bg(word) } else { plain })
        }),
    );
    spans
}

/// The words of `old` and `new` that differ, as runs of text flagged
/// changed or not. Lines with no word in common are left unmarked, since
/// marking all of them says nothing the color does not.
fn word_diff(old: &str, new: &str) -> (Marked, Marked) {
    const MAX_TOKENS: usize = 400;
    let unmarked = || (vec![(old.to_owned(), false)], vec![(new.to_owned(), false)]);
    let (a, b) = (tokens(old), tokens(new));
    if a.len() > MAX_TOKENS || b.len() > MAX_TOKENS {
        return unmarked();
    }
    // Longest common subsequence of the tokens, filled from the end.
    let mut table = vec![vec![0u16; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut kept_a, mut kept_b) = (vec![false; a.len()], vec![false; b.len()]);
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            kept_a[i] = true;
            kept_b[j] = true;
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let shared_word = a
        .iter()
        .zip(&kept_a)
        .any(|(token, kept)| *kept && !token.trim().is_empty());
    if !shared_word || (kept_a.iter().all(|kept| *kept) && kept_b.iter().all(|kept| *kept)) {
        return unmarked();
    }
    (runs(&a, &kept_a), runs(&b, &kept_b))
}

/// `text` split into words, runs of spaces, and single other characters.
fn tokens(text: &str) -> Vec<&str> {
    let kind = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            0
        } else if c.is_whitespace() {
            1
        } else {
            2
        }
    };
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut last = None;
    for (index, c) in text.char_indices() {
        let current = kind(c);
        if index > start && (current == 2 || last != Some(current)) {
            tokens.push(&text[start..index]);
            start = index;
        }
        last = Some(current);
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

/// Tokens joined into runs that are all kept or all changed. A space
/// between two changed tokens counts as changed, so a changed phrase reads
/// as one mark.
fn runs(tokens: &[&str], kept: &[bool]) -> Marked {
    let changed: Vec<bool> = (0..tokens.len())
        .map(|index| {
            !kept[index]
                || (tokens[index].trim().is_empty()
                    && index > 0
                    && index + 1 < tokens.len()
                    && !kept[index - 1]
                    && !kept[index + 1])
        })
        .collect();
    let mut runs: Marked = Vec::new();
    for (token, changed) in tokens.iter().zip(changed) {
        match runs.last_mut() {
            Some((text, flag)) if *flag == changed => text.push_str(token),
            _ => runs.push(((*token).to_owned(), changed)),
        }
    }
    runs
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

/// A diff line whose background runs to the end of the row, like a diff
/// viewer's, even when its text is shorter.
fn filled_row(spans: Vec<Span<'static>>, width: usize, background: Color) -> Line<'static> {
    let mut line = diff_row(spans, width);
    let used: usize = line.spans.iter().map(Span::width).sum();
    let end = width + 2 + GUTTER;
    if used < end {
        line.spans.push(Span::styled(
            " ".repeat(end - used),
            Style::default().bg(background),
        ));
    }
    line
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

/// What a collapsed call previews: a program's output, or its errors when
/// it printed nothing else, and otherwise the call's text.
fn preview_text(entry: &Entry) -> &str {
    match entry.output.as_ref().and_then(run_output) {
        Some(run) if !run.stdout.trim().is_empty() => run.stdout,
        Some(run) if !run.stderr.trim().is_empty() => run.stderr,
        _ => &entry.body,
    }
}

/// The first lines of a collapsed call's text, muted and cut to one row each.
fn output_preview(body: &str, width: usize, rows: usize) -> Vec<Line<'static>> {
    let text: Vec<&str> = body
        .lines()
        .map(str::trim_end)
        .skip_while(|line| line.trim().is_empty())
        .collect();
    let end = text
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |last| last + 1);
    let hidden = end.saturating_sub(rows);
    let mut lines: Vec<Line<'static>> = text[..end.min(rows)]
        .iter()
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
        .collect();
    if hidden > 0 {
        let noun = if hidden == 1 { "line" } else { "lines" };
        lines.push(indented(Line::styled(
            format!("  … {hidden} more {noun}"),
            Style::default().fg(MUTED),
        )));
    }
    lines
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
    clickable_at(app, screen, x, y).map(|(_, row)| row)
}

/// The clickable row at a screen cell: its conversation line, and what
/// clicking it opens.
pub(in crate::tui) fn clickable_at(
    app: &App,
    screen: Rect,
    x: u16,
    y: u16,
) -> Option<(usize, ToolRow)> {
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    if x < inner.x || x >= inner.right() || y < inner.y || y >= inner.bottom() {
        return None;
    }
    let conversation = conversation_lines(app, inner.width.saturating_sub(1).max(1) as usize);
    let line = conversation.offset(inner.height, app.scroll) + usize::from(y - inner.y);
    conversation
        .tool_headers
        .into_iter()
        .find(|(header, _)| line == *header)
}

/// Marks a clickable row as under the mouse, in the same color an open row
/// keeps.
fn hovered(line: &mut Line<'static>) {
    active(line);
}

/// A row's title and its open or closed marker in the active color. The
/// argument names and values after them keep their own colors.
fn active(line: &mut Line<'static>) {
    for span in &mut line.spans {
        let text = span.content.trim();
        if matches!(text, "" | "✦" | "•" | "│") {
            continue;
        }
        span.style = span.style.fg(ACTIVE);
        if matches!(text, OPEN | CLOSED) {
            return;
        }
    }
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
        assert!(rendered.contains("✦  git.status ▶"));
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
        assert!(text.contains("file"), "{text}");
        assert!(text.contains("/home/me/project/crates"), "{text}");
        assert!(text.contains("versation.rs"), "{text}");
        assert!(!text.contains("content"), "{text}");
        assert!(
            !text.contains("fn main() {"),
            "collapsed rows do not preview long arguments"
        );
        assert!(!text.contains("result"));

        app.entries[0].set_expanded(true);
        let text = rendered(&app, 50, 20);
        assert!(!text.contains("content"), "{text}");
        assert!(text.contains("│ +}"), "a write shows as a diff: {text}");
        assert!(!text.contains("│ Done"), "{text}");
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

    /// The rendered lines as text, without the background fill that runs
    /// added and removed diff lines to the end of the row.
    fn plain_lines(app: &App, width: usize) -> Vec<String> {
        conversation_lines(app, width)
            .lines
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
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
    fn an_edit_header_names_its_file_and_drops_the_text_sizes() {
        let mut app = changed_call();
        app.entries[0].input = Some(json!({
            "path": "src/a.rs",
            "old": "a\nx\n",
            "new": "b\nc\ny\nz\n"
        }));
        let header = plain_lines(&app, 80)[0].clone();
        assert!(header.contains("file src/a.rs"), "{header}");
        assert!(!header.contains("path"), "{header}");
        assert!(!header.contains("lines"), "{header}");
    }

    #[test]
    fn results_leave_out_the_revision_and_repeated_arguments() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("notes.save", "");
        app.entries[0].input = Some(json!({ "path": "n.md" }));
        app.entries[0].output = Some(json!({ "path": "n.md", "revision": 4, "words": 12 }));
        app.entries[0].expanded = true;
        let text = plain_lines(&app, 60).join("\n");
        assert!(text.contains("12"), "{text}");
        assert!(!text.contains("Revision"), "{text}");
        assert_eq!(text.matches("n.md").count(), 1, "{text}");
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
    fn long_arguments_are_left_off_the_header() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "");
        app.entries[0].input = Some(json!({ "path": "a.rs", "content": "x\ny\nz" }));
        app.entries[0].set_expanded(false);
        let lines = plain_lines(&app, 60);
        assert!(!lines[0].contains("content"), "{lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("± a.rs  +3 -0")),
            "a write saved without its changes still shows as one: {lines:?}"
        );

        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("notes.save", "");
        app.entries[0].input = Some(json!({ "content": "y".repeat(250) }));
        let lines = plain_lines(&app, 60);
        assert!(!lines[0].contains("content"), "{lines:?}");
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
            position("│ -a"),
            position("│ +b"),
            position("│ +c"),
        ];
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{lines:?}");
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("@@") || line.contains("Patched")),
            "hunk headers and the receipt are left out: {lines:?}"
        );
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
        assert!(added.iter().all(|span| span.style.bg == Some(ADDED_LINE)));
        let removed = after_bar(&diff_line(&app, 60, "│ -a"));
        assert_eq!(removed[0].content, "-");
        assert!(
            removed
                .iter()
                .all(|span| span.style.bg == Some(REMOVED_LINE))
        );
        let width = |line: &Line<'static>| line.spans.iter().map(Span::width).sum::<usize>();
        assert_eq!(
            width(&diff_line(&app, 60, "│ +b")),
            width(&diff_line(&app, 60, "│ -a")),
            "the background runs to the end of every row"
        );
    }

    #[test]
    fn diff_code_is_highlighted_by_file_type() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "");
        app.entries[0].set_changes(vec![change(
            "x.rs",
            "modified",
            Some("@@ -1,2 +1,2 @@\n fn keep() {}\n-fn e() {}\n+fn f() {}\n"),
        )]);
        app.entries[0].set_expanded(true);
        let spans = after_bar(&diff_line(&app, 60, "│  fn keep() {}"));
        assert!(
            spans[1..].iter().any(|span| span.style.fg.is_some()),
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
    fn expanded_changed_calls_show_only_the_diff() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("fs.write", "Done");
        app.entries[0].input = Some(json!({
            "path": "a.txt",
            "content": "one\ntwo\nthree",
            "old": "zero\none",
            "new": "one\ntwo",
        }));
        app.entries[0].output = Some(json!({ "path": "a.txt", "revision": 4 }));
        app.entries[0].set_changes(vec![change(
            "a.txt",
            "created",
            Some("@@ -0,0 +1,3 @@\n+one\n+two\n+three\n"),
        )]);
        app.entries[0].set_expanded(true);
        let lines = plain_lines(&app, 60);
        assert!(!lines[0].contains("content"), "{lines:?}");
        assert!(
            lines.iter().any(|line| line.ends_with("│ +two")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.ends_with("│ two")),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| ["result", "new", "old", "content"].contains(&line.trim())),
            "{lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.ends_with("│ Done") || line.contains("Revision")),
            "{lines:?}"
        );
    }

    #[test]
    fn every_line_of_a_call_toggles_it() {
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
            if expanded {
                assert!(
                    conversation
                        .tool_headers
                        .iter()
                        .any(|(line, _)| conversation.lines[*line].to_string().contains("│ +b")),
                    "an open call closes from its diff too"
                );
            }
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
        assert_eq!(removed[0].style.bg, Some(REMOVED_LINE));
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
        assert!(lines[0].contains("✦  git.status ▶"), "{lines:?}");
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
        assert_eq!(text.len(), 4);
        assert!(text[0].ends_with("xxxxxxxxx…"), "{}", text[0]);
        assert!(text[2].ends_with('c'));
        assert!(text[3].ends_with("… 1 more line"), "{}", text[3]);
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
        assert!(text.contains("✦  git.status ▶"));
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
        assert!(closed.contains("Used 2 actions and 1 tool ▶"), "{closed}");
        assert!(!closed.contains("git.diff"), "{closed}");

        let screen = Rect::new(0, 0, 60, 24);
        toggle_tool(&mut app, screen, ToolRow::Group(1));
        let open = text(&app);
        assert!(open.contains("Used 2 actions and 1 tool ▼"), "{open}");
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
        terminal
            .draw(|frame| super::super::draw(frame, app))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let cells = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect()
            })
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
            numbered
                .lines()
                .map(|line| format!(" {line}\n"))
                .collect::<String>()
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
                assert!(
                    line.width() <= text_width,
                    "{} > {text_width}: {line}",
                    line.width()
                );
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
            terminal
                .draw(|frame| super::super::draw(frame, &app))
                .unwrap();
            // The terminal kept only the cells each draw changed; they must
            // add up to what a fresh draw of the same state shows.
            let (_, fresh) = screen(&app, width, height);
            let kept = terminal.backend().buffer();
            for y in 0..height {
                let last = &kept[(width - 1, y)];
                assert!(
                    last.symbol().width() <= 1,
                    "wide cell on the edge at row {y}"
                );
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
        terminal
            .draw(|frame| super::super::draw(frame, &app))
            .unwrap();
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

    #[test]
    fn paired_lines_mark_only_the_changed_words() {
        let app = expanded_diff(
            "x.txt",
            "@@ -1 +1 @@\n-let total = price * count;\n+let total = cost * count;\n",
        );
        let marked = |needle: &str, word: Color| -> Vec<String> {
            after_bar(&diff_line(&app, 60, needle))
                .iter()
                .filter(|span| span.style.bg == Some(word))
                .map(|span| span.content.to_string())
                .collect()
        };
        assert_eq!(
            marked("│ -let total = price * count;", REMOVED_WORD),
            ["price"]
        );
        assert_eq!(marked("│ +let total = cost * count;", ADDED_WORD), ["cost"]);
        let unpaired = expanded_diff("x.txt", "@@ -0,0 +1 @@\n+fresh line\n");
        let spans = after_bar(&diff_line(&unpaired, 60, "│ +fresh line"));
        assert!(
            spans.iter().all(|span| span.style.bg == Some(ADDED_LINE)),
            "{spans:?}"
        );
    }

    #[test]
    fn program_runs_show_problems_first_and_output_last() {
        let run = |output: Value| {
            let mut app = App::new("http://127.0.0.1:7777", 1000, None);
            app.push_tool("shell", "flat text");
            app.entries[0].output = Some(output);
            app.entries[0].set_expanded(true);
            plain_lines(&app, 60)
                .into_iter()
                .map(|line| line.trim().to_owned())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
        };
        let clean = run(json!({
            "stdout": "hello\nworld\n",
            "stderr": "",
            "exit_code": 0,
            "timed_out": false,
            "truncated": false,
        }));
        assert_eq!(clean[1..], ["output", "│ hello", "│ world"], "{clean:?}");

        let failed = run(json!({
            "stdout": "partial\n",
            "stderr": "boom\n",
            "exit_code": 2,
            "timed_out": true,
            "truncated": true,
        }));
        assert_eq!(
            failed[1..],
            [
                "exit code 2",
                "stopped: ran too long",
                "output cut",
                "errors",
                "│ boom",
                "output",
                "│ partial",
            ],
            "{failed:?}"
        );
    }

    #[test]
    fn other_results_mute_their_labels() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push_tool("kv.get", "Key: a");
        app.entries[0].output = Some(json!({ "key": "a", "found": true }));
        app.entries[0].set_expanded(true);
        let line = diff_line(&app, 60, "│ Key  a");
        let label = line
            .spans
            .iter()
            .find(|span| span.content == "Key")
            .unwrap();
        assert_eq!(label.style.fg, Some(MUTED));
        let value = line.spans.iter().find(|span| span.content == "a").unwrap();
        assert_eq!(value.style.fg, None);
        diff_line(&app, 60, "│ Found  Yes");
    }
}

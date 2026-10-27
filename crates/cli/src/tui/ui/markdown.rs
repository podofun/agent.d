use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy)]
pub(in crate::tui) struct Palette {
    pub primary: Color,
    pub secondary: Color,
    pub muted: Color,
}

pub(super) fn render(text: &str, width: usize, palette: Palette) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(width.max(1), palette);
    for event in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES) {
        renderer.event(event);
    }
    renderer.finish()
}

pub(super) fn wrap_plain(text: &str, width: usize) -> Vec<Line<'static>> {
    text.split('\n')
        .flat_map(|line| wrap_spans(vec![Span::raw(line.to_owned())], width))
        .collect()
}

/// Wraps styled spans on spaces, keeping each span's style. A word wider than
/// the line is broken by grapheme.
pub(super) fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in spans {
        let style = span.style;
        for word in span.content.split_inclusive(' ') {
            let trimmed = word.trim_end_matches(' ');
            let word_width = trimmed.width();
            if used > 0 && used + word_width > width {
                lines.push(Line::from(trim_trailing_space(std::mem::take(
                    &mut current,
                ))));
                used = 0;
            }
            if word_width > width {
                let mut piece = String::new();
                let mut piece_width = 0;
                for grapheme in trimmed.graphemes(true) {
                    let grapheme_width = grapheme.width();
                    if used + piece_width + grapheme_width > width {
                        current.push(Span::styled(std::mem::take(&mut piece), style));
                        lines.push(Line::from(std::mem::take(&mut current)));
                        used = 0;
                        piece_width = 0;
                    }
                    piece.push_str(grapheme);
                    piece_width += grapheme_width;
                }
                current.push(Span::styled(piece, style));
                used += piece_width;
            } else if !trimmed.is_empty() {
                current.push(Span::styled(trimmed.to_owned(), style));
                used += word_width;
            }
            if word.ends_with(' ') && used < width {
                current.push(Span::styled(" ".to_owned(), style));
                used += 1;
            }
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(Line::from(trim_trailing_space(current)));
    }
    lines
}

/// Column widths within `room`: each column its natural width when all fit.
/// Otherwise every column first gets its longest word, so words stay whole,
/// and the rest is shared. When even the words do not fit, narrow columns
/// keep their width and wide ones share what is left. `None` when a column
/// would get less than `MIN_COLUMN`.
fn fit_columns(natural: &[usize], words: &[usize], room: usize) -> Option<Vec<usize>> {
    if natural.iter().sum::<usize>() <= room {
        return Some(natural.to_vec());
    }
    let floors: Vec<usize> = natural
        .iter()
        .zip(words)
        .map(|(natural, word)| (*word).clamp(MIN_COLUMN.min(*natural), *natural))
        .collect();
    let floor: usize = floors.iter().sum();
    if floor <= room {
        let mut order: Vec<usize> = (0..natural.len()).collect();
        order.sort_by_key(|&column| natural[column] - floors[column]);
        let mut widths = floors.clone();
        let mut left = room - floor;
        for (done, &column) in order.iter().enumerate() {
            let share = left / (natural.len() - done);
            let extra = (natural[column] - floors[column]).min(share);
            widths[column] += extra;
            left -= extra;
        }
        return Some(widths);
    }
    let mut order: Vec<usize> = (0..natural.len()).collect();
    order.sort_by_key(|&column| natural[column]);
    let mut widths = vec![0; natural.len()];
    let mut left = room;
    for (done, &column) in order.iter().enumerate() {
        let share = left / (natural.len() - done);
        widths[column] = natural[column].min(share);
        left -= widths[column];
    }
    widths
        .iter()
        .zip(natural)
        .all(|(width, natural)| *width >= MIN_COLUMN.min(*natural))
        .then_some(widths)
}

fn trim_trailing_space(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    while spans
        .last()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.pop();
    }
    spans
}

/// A table being read: its column alignments, header cells, and rows.
struct Table {
    aligns: Vec<Alignment>,
    head: Vec<Vec<Span<'static>>>,
    rows: Vec<Vec<Vec<Span<'static>>>>,
    row: Vec<Vec<Span<'static>>>,
}

/// Narrowest a column gets before the table is shown as records instead.
const MIN_COLUMN: usize = 6;

enum ListKind {
    Bullet,
    Ordered(u64),
}

struct Renderer {
    width: usize,
    palette: Palette,
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style: Style,
    lists: Vec<ListKind>,
    quote_depth: usize,
    code: Option<String>,
    /// The language tag of the code block being collected, if any.
    code_language: String,
    marker: Option<String>,
    table: Option<Table>,
}

impl Renderer {
    fn new(width: usize, palette: Palette) -> Self {
        Self {
            width,
            palette,
            lines: Vec::new(),
            spans: Vec::new(),
            style: Style::default(),
            lists: Vec::new(),
            quote_depth: 0,
            code: None,
            code_language: String::new(),
            marker: None,
            table: None,
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some(code) = &mut self.code {
                    code.push_str(&text);
                } else {
                    self.spans
                        .push(Span::styled(text.into_string(), self.style));
                }
            }
            Event::Code(code) => self.spans.push(Span::styled(
                code.into_string(),
                self.style.fg(self.palette.secondary),
            )),
            Event::SoftBreak => self.spans.push(Span::styled(" ".to_owned(), self.style)),
            Event::HardBreak => self.flush_paragraph(),
            Event::Rule => {
                self.blank_line();
                let indent = self.indent();
                let rule = "─".repeat(self.width.saturating_sub(indent.width()));
                self.lines.push(Line::from(vec![
                    Span::styled(indent, Style::default().fg(self.palette.muted)),
                    Span::styled(rule, Style::default().fg(self.palette.muted)),
                ]));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.spans
                    .push(Span::styled(html.into_string(), self.style));
            }
            Event::FootnoteReference(_)
            | Event::TaskListMarker(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph if self.marker.is_none() => self.blank_line(),
            Tag::Table(aligns) => {
                self.blank_line();
                self.table = Some(Table {
                    aligns,
                    head: Vec::new(),
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::Heading { level, .. } => {
                self.blank_line();
                let color = match level {
                    HeadingLevel::H1 | HeadingLevel::H2 => self.palette.primary,
                    _ => self.palette.secondary,
                };
                self.style = Style::default().fg(color).add_modifier(Modifier::BOLD);
            }
            Tag::BlockQuote(_) => {
                self.blank_line();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.blank_line();
                self.code = Some(String::new());
                self.code_language.clear();
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    self.push_code_line(&language, true);
                    // A tag may carry attributes after the language: `rust,ignore`.
                    self.code_language = language
                        .split([',', ' '])
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                }
            }
            Tag::List(start) => {
                if self.lists.is_empty() {
                    self.blank_line();
                } else {
                    self.flush_paragraph();
                }
                self.lists.push(match start {
                    Some(number) => ListKind::Ordered(number),
                    None => ListKind::Bullet,
                });
            }
            Tag::Item => {
                self.flush_paragraph();
                self.marker = Some(match self.lists.last_mut() {
                    Some(ListKind::Ordered(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                });
            }
            Tag::Emphasis => self.style = self.style.add_modifier(Modifier::ITALIC),
            Tag::Strong => self.style = self.style.add_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
            Tag::Link { .. } => self.style = self.style.add_modifier(Modifier::UNDERLINED),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Item => self.flush_paragraph(),
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                if let Some(table) = &mut self.table {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head = std::mem::take(&mut table.row);
                }
            }
            TagEnd::TableRow => {
                if let Some(table) = &mut self.table {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.push_table(table);
                }
            }
            TagEnd::Heading(_) => {
                self.flush_paragraph();
                self.style = Style::default();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_paragraph();
                self.quote_depth -= 1;
            }
            TagEnd::CodeBlock => {
                if let Some(code) = self.code.take() {
                    let code = code.trim_end_matches('\n');
                    match super::highlight::highlight(code, &self.code_language) {
                        Some(lines) => {
                            for spans in lines {
                                self.push_code_spans(spans);
                            }
                        }
                        None => {
                            for line in code.split('\n') {
                                self.push_code_line(line, false);
                            }
                        }
                    }
                }
            }
            TagEnd::List(_) => {
                self.flush_paragraph();
                self.lists.pop();
            }
            TagEnd::Emphasis => self.style = self.style.remove_modifier(Modifier::ITALIC),
            TagEnd::Strong => self.style = self.style.remove_modifier(Modifier::BOLD),
            TagEnd::Strikethrough => self.style = self.style.remove_modifier(Modifier::CROSSED_OUT),
            TagEnd::Link => self.style = self.style.remove_modifier(Modifier::UNDERLINED),
            _ => {}
        }
    }

    fn indent(&self) -> String {
        let mut indent = "┃ ".repeat(self.quote_depth);
        indent.push_str(&"  ".repeat(self.lists.len().saturating_sub(1)));
        indent
    }

    fn blank_line(&mut self) {
        self.flush_paragraph();
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(Line::raw(""));
        }
    }

    fn flush_paragraph(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let indent = self.indent();
        let marker = self.marker.take().unwrap_or_default();
        let first = format!("{indent}{marker}");
        let continuation = format!("{indent}{}", " ".repeat(marker.width()));
        let prefix_style = Style::default().fg(self.palette.muted);
        let body_width = self.width.saturating_sub(first.width()).max(1);
        let wrapped = wrap_spans(std::mem::take(&mut self.spans), body_width);
        for (index, mut line) in wrapped.into_iter().enumerate() {
            let prefix = if index == 0 { &first } else { &continuation };
            if !prefix.is_empty() {
                line.spans
                    .insert(0, Span::styled(prefix.clone(), prefix_style));
            }
            self.lines.push(line);
        }
    }

    /// Columns sized to their content and separated by `│`. When the table
    /// is too wide, the widest columns wrap; when even that leaves a column
    /// narrower than `MIN_COLUMN`, each row is shown as `header: value` lines.
    fn push_table(&mut self, table: Table) {
        let indent = self.indent();
        let room = self.width.saturating_sub(indent.width());
        let columns = table
            .rows
            .iter()
            .chain(std::iter::once(&table.head))
            .map(Vec::len)
            .max()
            .unwrap_or(0);
        if columns == 0 {
            return;
        }
        let cell_width =
            |cell: &Vec<Span<'static>>| cell.iter().map(|s| s.content.width()).sum::<usize>();
        let natural: Vec<usize> = (0..columns)
            .map(|column| {
                std::iter::once(&table.head)
                    .chain(&table.rows)
                    .filter_map(|row| row.get(column))
                    .map(cell_width)
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        let muted = Style::default().fg(self.palette.muted);
        let longest_word: Vec<usize> = (0..columns)
            .map(|column| {
                std::iter::once(&table.head)
                    .chain(&table.rows)
                    .filter_map(|row| row.get(column))
                    .flat_map(|cell| cell.iter())
                    .flat_map(|span| span.content.split_whitespace())
                    .map(UnicodeWidthStr::width)
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let Some(widths) = fit_columns(
            &natural,
            &longest_word,
            room.saturating_sub(3 * (columns - 1)),
        ) else {
            self.push_records(&table, &indent, room);
            return;
        };
        let bold = |cell: &Vec<Span<'static>>| {
            cell.iter()
                .map(|span| span.clone().patch_style(Modifier::BOLD))
                .collect::<Vec<_>>()
        };
        let mut rows: Vec<(Vec<Vec<Span<'static>>>, bool)> = Vec::new();
        if !table.head.is_empty() {
            rows.push((table.head.iter().map(bold).collect(), true));
        }
        rows.extend(table.rows.iter().map(|row| (row.clone(), false)));
        let rule = widths
            .iter()
            .map(|width| "─".repeat(*width))
            .collect::<Vec<_>>()
            .join("─┼─");
        let rule = Line::styled(format!("{indent}{rule}"), muted);
        for (index, (row, _)) in rows.into_iter().enumerate() {
            // A rule between rows, the one under the header included.
            if index > 0 {
                self.lines.push(rule.clone());
            }
            let cells: Vec<Vec<Line<'static>>> = (0..columns)
                .map(|column| {
                    let cell = row.get(column).cloned().unwrap_or_default();
                    wrap_spans(cell, widths[column])
                })
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for line_index in 0..height {
                let mut spans = vec![Span::styled(indent.clone(), muted)];
                for (column, cell) in cells.iter().enumerate() {
                    if column > 0 {
                        spans.push(Span::styled(" │ ", muted));
                    }
                    let line = cell.get(line_index).cloned().unwrap_or_default();
                    let pad = widths[column].saturating_sub(line.width());
                    let right = matches!(table.aligns.get(column), Some(Alignment::Right));
                    if right {
                        spans.push(Span::raw(" ".repeat(pad)));
                    }
                    spans.extend(line.spans);
                    if !right && column + 1 < columns {
                        spans.push(Span::raw(" ".repeat(pad)));
                    }
                }
                self.lines.push(Line::from(trim_trailing_space(spans)));
            }
        }
    }

    /// A table too wide for its columns: each row as `header: value` lines,
    /// with a blank line between rows.
    fn push_records(&mut self, table: &Table, indent: &str, room: usize) {
        let muted = Style::default().fg(self.palette.muted);
        for (index, row) in table.rows.iter().enumerate() {
            if index > 0 {
                self.lines.push(Line::raw(""));
            }
            for (column, cell) in row.iter().enumerate() {
                let label = table
                    .head
                    .get(column)
                    .map(|head| {
                        head.iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                let mut spans = Vec::new();
                if !label.is_empty() {
                    spans.push(Span::styled(format!("{label}: "), muted));
                }
                spans.extend(cell.iter().cloned());
                for (line_index, mut line) in wrap_spans(spans, room.max(1)).into_iter().enumerate()
                {
                    let prefix = if line_index == 0 {
                        indent.to_owned()
                    } else {
                        format!("{indent}  ")
                    };
                    line.spans.insert(0, Span::styled(prefix, muted));
                    self.lines.push(line);
                }
            }
        }
    }

    fn push_code_line(&mut self, line: &str, muted: bool) {
        let style = if muted {
            Style::default().fg(self.palette.muted)
        } else {
            Style::default()
        };
        self.push_code_spans(vec![Span::styled(line.to_owned(), style)]);
    }

    /// One code line: the muted `│ ` prefix, then `spans` cut to the width
    /// by grapheme, never wrapped.
    fn push_code_spans(&mut self, spans: Vec<Span<'static>>) {
        let prefix = format!("{}│ ", self.indent());
        let mut used = prefix.width();
        let mut line = vec![Span::styled(
            prefix,
            Style::default().fg(self.palette.muted),
        )];
        'spans: for span in spans {
            let mut body = String::new();
            for grapheme in span.content.graphemes(true) {
                let grapheme_width = grapheme.width();
                if used + grapheme_width > self.width {
                    if !body.is_empty() {
                        line.push(Span::styled(body, span.style));
                    }
                    break 'spans;
                }
                body.push_str(grapheme);
                used += grapheme_width;
            }
            line.push(Span::styled(body, span.style));
        }
        if line.len() == 1 {
            line.push(Span::raw(String::new()));
        }
        self.lines.push(Line::from(line));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_paragraph();
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        while self.lines.first().is_some_and(|line| line.spans.is_empty()) {
            self.lines.remove(0);
        }
        if self.lines.is_empty() {
            self.lines.push(Line::raw(""));
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn palette() -> Palette {
        Palette {
            primary: Color::Magenta,
            secondary: Color::Blue,
            muted: Color::DarkGray,
        }
    }

    #[test]
    fn headings_and_inline_styles_lose_their_markers() {
        let lines = render("# Title\n\nSome **bold** and `code` here.", 40, palette());
        let text = text_of(&lines);
        assert_eq!(text[0], "Title");
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(text[2], "Some bold and code here.");
        let bold = lines[2]
            .spans
            .iter()
            .find(|span| span.content == "bold")
            .unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let code = lines[2]
            .spans
            .iter()
            .find(|span| span.content == "code")
            .unwrap();
        assert_eq!(code.style.fg, Some(Color::Blue));
    }

    #[test]
    fn fenced_blocks_are_prefixed_and_truncated_not_wrapped() {
        let lines = render(
            "```rust\nlet x = 1;\nlet very_long_identifier_name = 2;\n```",
            20,
            palette(),
        );
        let text = text_of(&lines);
        assert_eq!(text[0], "│ rust");
        assert_eq!(text[1], "│ let x = 1;");
        assert_eq!(text[2].chars().count(), 20);
        assert_eq!(text.len(), 3);
    }

    #[test]
    fn fenced_code_is_highlighted() {
        let lines = render("```rust\nlet x = 1;\n```", 40, palette());
        assert_eq!(text_of(&lines)[1], "│ let x = 1;");
        assert!(
            lines[1].spans[1..]
                .iter()
                .any(|span| span.style.fg.is_some() && span.style.fg != Some(palette().muted)),
            "{:?}",
            lines[1]
        );
    }

    #[test]
    fn unknown_fences_render_plain() {
        let lines = render("```nolang\nx\n```", 40, palette());
        assert_eq!(lines[1].spans.len(), 2);
        assert_eq!(lines[1].spans[1].content, "x");
        assert_eq!(lines[1].spans[1].style, Style::default());
    }

    #[test]
    fn highlighted_lines_truncate_by_grapheme() {
        let lines = render(
            "```rust\nlet s = \"日本語日本語日本語\";\n```",
            16,
            palette(),
        );
        let line = &text_of(&lines)[1];
        assert!(line.width() <= 16, "{line}");
        assert!(line.starts_with("│ let s = \"日"), "{line}");
    }

    #[test]
    fn lists_are_bulleted_numbered_and_nested() {
        let lines = render(
            "- one\n- two\n  - inner\n\n1. first\n2. second",
            40,
            palette(),
        );
        let text = text_of(&lines);
        assert_eq!(text[0], "• one");
        assert_eq!(text[1], "• two");
        assert_eq!(text[2], "  • inner");
        assert_eq!(text[4], "1. first");
        assert_eq!(text[5], "2. second");
    }

    #[test]
    fn paragraphs_wrap_on_words_and_break_long_tokens() {
        let lines = wrap_plain("hello wonderful world", 10);
        assert_eq!(text_of(&lines), vec!["hello", "wonderful", "world"]);
        let lines = wrap_plain("abcdefghijklmnop", 6);
        assert_eq!(text_of(&lines), vec!["abcdef", "ghijkl", "mnop"]);
        let lines = render(
            "A paragraph with a **bold word** that wraps.",
            16,
            palette(),
        );
        assert_eq!(
            text_of(&lines),
            vec!["A paragraph with", "a bold word that", "wraps."]
        );
    }

    #[test]
    fn quotes_rules_and_links_render_as_text() {
        let lines = render(
            "> quoted\n\n---\n\n[docs](https://example.com) and https://x.y",
            12,
            palette(),
        );
        let text = text_of(&lines);
        assert_eq!(text[0], "┃ quoted");
        assert_eq!(text[2], "─".repeat(12));
        assert!(text[4].starts_with("docs and"));
    }

    #[test]
    fn zero_width_never_panics() {
        assert!(!render("# hi\n\n- a", 0, palette()).is_empty());
        assert!(wrap_plain("x", 0).len() == 1);
    }

    #[test]
    fn tables_line_up_their_columns() {
        let table = "| # | File | Change |\n|---|---|---|\n| 1 | `a.rs` | new |\n| 12 | b.rs | removed one test |";
        let lines = text_of(&render(table, 60, palette()));
        assert_eq!(
            lines,
            [
                "#  │ File │ Change",
                "───┼──────┼─────────────────",
                "1  │ a.rs │ new",
                "───┼──────┼─────────────────",
                "12 │ b.rs │ removed one test",
            ]
        );
    }

    #[test]
    fn a_narrow_table_wraps_its_widest_column() {
        let table = "| # | What changed |\n|---|---|\n| 1 | the tool rows now render their input and output |";
        let lines = text_of(&render(table, 24, palette()));
        assert_eq!(lines[0], "# │ What changed");
        assert!(lines.len() > 3, "{lines:?}");
        assert!(lines.iter().all(|line| line.width() <= 24), "{lines:?}");
        assert!(lines[2].starts_with("1 │ the tool"), "{lines:?}");
        assert!(lines[3].starts_with("  │ "), "{lines:?}");
    }

    #[test]
    fn a_table_too_wide_for_its_columns_becomes_records() {
        let table = "| Alpha | Beta | Gamma | Delta |\n|---|---|---|---|\n| one value | two value | three value | four value |";
        let lines = text_of(&render(table, 20, palette()));
        assert_eq!(lines[0], "Alpha: one value");
        assert!(
            lines.iter().any(|line| line.starts_with("Delta: ")),
            "{lines:?}"
        );
    }

    #[test]
    fn shrinking_columns_keeps_paths_whole() {
        let table = "| File | What changed |\n|---|---|\n| crates/cli/src/tui/presentation.rs | the old tool call helper is gone now |";
        let lines = text_of(&render(table, 60, palette()));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("crates/cli/src/tui/presentation.rs")),
            "{lines:?}"
        );
        assert!(lines.iter().all(|line| line.width() <= 60), "{lines:?}");
    }
}

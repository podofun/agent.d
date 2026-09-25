use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
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
    for event in Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH) {
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

fn trim_trailing_space(mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    while spans
        .last()
        .is_some_and(|span| span.content.trim().is_empty())
    {
        spans.pop();
    }
    spans
}

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
    marker: Option<String>,
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
            marker: None,
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
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    self.push_code_line(&language, true);
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
                    for line in code.trim_end_matches('\n').split('\n') {
                        self.push_code_line(line, false);
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

    fn push_code_line(&mut self, line: &str, muted: bool) {
        let prefix = format!("{}│ ", self.indent());
        let style = if muted {
            Style::default().fg(self.palette.muted)
        } else {
            Style::default()
        };
        let mut used = prefix.width();
        let mut body = String::new();
        for grapheme in line.graphemes(true) {
            let grapheme_width = grapheme.width();
            if used + grapheme_width > self.width {
                break;
            }
            body.push_str(grapheme);
            used += grapheme_width;
        }
        self.lines.push(Line::from(vec![
            Span::styled(prefix, Style::default().fg(self.palette.muted)),
            Span::styled(body, style),
        ]));
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
}

//! Mouse selection over the conversation. Points live in conversation line
//! coordinates, so a selection survives scrolling; only the selectable
//! column range of each line (never gutters, tool summaries, or the `│ `
//! prefix) is highlighted or copied.

use std::ops::Range;

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::super::app::App;
use super::conversation::{Conversation, conversation_for, conversation_inner};
use super::screen_rows;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(in crate::tui) struct Point {
    pub(in crate::tui) line: usize,
    pub(in crate::tui) col: usize,
}

#[derive(Clone, Copy, Debug)]
pub(in crate::tui) struct Selection {
    pub(in crate::tui) anchor: Point,
    pub(in crate::tui) head: Point,
}

impl Selection {
    pub(in crate::tui) fn at(point: Point) -> Self {
        Self {
            anchor: point,
            head: point,
        }
    }

    /// True until the mouse moved to another cell.
    pub(in crate::tui) fn is_click(&self) -> bool {
        self.anchor == self.head
    }

    fn ordered(&self) -> (Point, Point) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    /// Selected columns of `line`, or an empty range when the line is
    /// outside the selection. Both end cells are included.
    fn columns(&self, line: usize) -> Range<usize> {
        let (start, end) = self.ordered();
        if line < start.line || line > end.line {
            return 0..0;
        }
        let from = if line == start.line { start.col } else { 0 };
        let to = if line == end.line {
            end.col + 1
        } else {
            usize::MAX
        };
        from..to
    }
}

/// The conversation cell under screen position (`x`, `y`), if any.
pub(in crate::tui) fn point_at(app: &App, screen: Rect, x: u16, y: u16) -> Option<Point> {
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    if x < inner.x || x >= inner.right() || y < inner.y || y >= inner.bottom() {
        return None;
    }
    let conversation = conversation_for(app, inner.width.saturating_sub(1).max(1) as usize);
    let line = conversation.offset(inner.height, app.scroll) + usize::from(y - inner.y);
    (line < conversation.lines.len()).then_some(Point {
        line,
        col: usize::from(x - inner.x),
    })
}

/// The selected text at the conversation width `screen` gives.
pub(in crate::tui) fn selected_text(app: &App, screen: Rect, selection: &Selection) -> String {
    let inner = conversation_inner(screen_rows(screen, app)[1]);
    let conversation = conversation_for(app, inner.width.saturating_sub(1).max(1) as usize);
    text(&conversation, selection)
}

/// The selected text, lines joined with newlines.
pub(in crate::tui) fn text(conversation: &Conversation, selection: &Selection) -> String {
    let (start, end) = selection.ordered();
    let last = end.line.min(conversation.lines.len().saturating_sub(1));
    let mut out = String::new();
    for index in start.line..=last {
        let range = intersect(
            selection.columns(index),
            conversation.selectable[index].clone(),
        );
        let mut col = 0;
        for span in &conversation.lines[index].spans {
            for grapheme in span.content.graphemes(true) {
                let width = grapheme.width();
                if covers(&range, col, width) {
                    out.push_str(grapheme);
                }
                col += width;
            }
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_owned()
}

fn intersect(a: Range<usize>, b: Range<usize>) -> Range<usize> {
    let start = a.start.max(b.start);
    start..a.end.min(b.end).max(start)
}

/// Whether a cell run at `col` of `width` columns falls inside `range`.
/// Zero-width graphemes follow the cell they attach to.
fn covers(range: &Range<usize>, col: usize, width: usize) -> bool {
    col < range.end && col + width.max(1) > range.start
}

/// Reverse the selected cells of `lines` in place.
pub(super) fn highlight(
    lines: &mut [Line<'static>],
    selectable: &[Range<usize>],
    selection: &Selection,
) {
    let (start, end) = selection.ordered();
    for index in start.line..=end.line.min(lines.len().saturating_sub(1)) {
        let range = intersect(selection.columns(index), selectable[index].clone());
        if range.is_empty() {
            continue;
        }
        let mut col = 0;
        let mut spans = Vec::new();
        for span in lines[index].spans.drain(..) {
            let mut run = String::new();
            let mut run_selected = None;
            for grapheme in span.content.graphemes(true) {
                let width = grapheme.width();
                let selected = covers(&range, col, width);
                if run_selected.is_some_and(|current| current != selected) {
                    spans.push(styled(std::mem::take(&mut run), span.style, run_selected));
                }
                run_selected = Some(selected);
                run.push_str(grapheme);
                col += width;
            }
            if !run.is_empty() {
                spans.push(styled(run, span.style, run_selected));
            }
        }
        lines[index].spans = spans;
    }
}

fn styled(text: String, style: ratatui::style::Style, selected: Option<bool>) -> Span<'static> {
    let style = if selected == Some(true) {
        style.add_modifier(Modifier::REVERSED)
    } else {
        style
    };
    Span::styled(text, style)
}

#[cfg(test)]
mod tests {
    use super::super::draw;
    use super::*;
    use crate::tui::app::EntryKind;
    use ratatui::Terminal;

    const GUTTER: usize = 3;

    fn app() -> App {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::User, "hello world");
        app.push(EntryKind::Agent, "reply here");
        app.push_tool("git.status", "line one\nline two");
        app.entries[2].set_expanded(true);
        app
    }

    fn conversation(app: &App, width: usize) -> Conversation {
        conversation_for(app, width)
    }

    #[test]
    fn text_skips_gutters_summaries_and_prefixes() {
        let app = app();
        let conversation = conversation(&app, 60);
        let last = conversation.lines.len() - 1;
        let all = Selection {
            anchor: Point { line: 0, col: 1 },
            head: Point {
                line: last,
                col: 59,
            },
        };
        assert_eq!(
            text(&conversation, &all),
            "hello world\n\nreply here\n\ngit.status\nline one\nline two"
        );
    }

    #[test]
    fn text_clips_columns_by_display_width() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.push(EntryKind::Agent, "a界b");
        let conversation = conversation(&app, 60);
        let cell = |col: usize| Selection::at(Point { line: 0, col });
        assert_eq!(text(&conversation, &cell(GUTTER)), "a");
        assert_eq!(text(&conversation, &cell(GUTTER + 1)), "界");
        assert_eq!(text(&conversation, &cell(GUTTER + 2)), "界");
        assert_eq!(text(&conversation, &cell(GUTTER + 3)), "b");
        assert_eq!(text(&conversation, &cell(GUTTER + 9)), "");
    }

    #[test]
    fn point_at_maps_cells_through_the_scroll_offset() {
        let mut app = app();
        let screen = Rect::new(0, 0, 60, 8);
        let inner = conversation_inner(screen_rows(screen, &app)[1]);
        assert_eq!(point_at(&app, screen, 0, inner.y), None);
        let conversation = conversation(&app, inner.width as usize - 1);
        let bottom = conversation.lines.len() - 1;
        let last_row = inner.bottom() - 1;
        assert_eq!(
            point_at(&app, screen, inner.x + 4, last_row),
            Some(Point {
                line: bottom,
                col: 4
            })
        );
        app.scroll = 2;
        assert_eq!(
            point_at(&app, screen, inner.x + 4, last_row),
            Some(Point {
                line: bottom - 2,
                col: 4
            })
        );
        assert_eq!(point_at(&app, screen, inner.x, screen.height - 1), None);
    }

    #[test]
    fn status_bar_offers_copy_while_text_is_selected() {
        let mut app = app();
        app.selection = Some(Selection {
            anchor: Point { line: 0, col: 3 },
            head: Point { line: 0, col: 7 },
        });
        let backend = ratatui::backend::TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let status: String = (0..60).map(|x| buffer[(x, 23)].symbol()).collect();
        assert!(status.contains("Ctrl+C copy"), "{status}");
        assert!(status.contains("Esc clear"), "{status}");
    }

    #[test]
    fn highlight_reverses_text_but_never_the_gutter() {
        let mut app = app();
        app.selection = Some(Selection {
            anchor: Point { line: 0, col: 0 },
            head: Point { line: 0, col: 30 },
        });
        let backend = ratatui::backend::TestBackend::new(60, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..60).map(|x| buffer[(x, 2)].symbol()).collect();
        let column = |needle: &str| row[..row.find(needle).unwrap()].chars().count() as u16;
        let gutter = column("›");
        let hello = column("hello");
        let reversed = |x: u16| {
            buffer[(x, 2)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED)
        };
        assert!(!reversed(gutter));
        assert!(reversed(hello));
        assert!(reversed(hello + 10));
        assert!(!reversed(hello + 11), "padding after the text stays plain");
    }
}

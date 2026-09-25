use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Editor {
    text: String,
    cursor: usize,
    selection_anchor: Option<usize>,
    desired_column: Option<usize>,
}

pub(super) enum Edit {
    Consumed,
    Ignored,
    Notice(String),
}

pub(super) struct Segment {
    pub(super) text: String,
    pub(super) selected: bool,
}

pub(super) struct Layout {
    pub(super) lines: Vec<String>,
    pub(super) segments: Vec<Vec<Segment>>,
    pub(super) cursor_row: usize,
    pub(super) cursor_column: usize,
    boundaries: Vec<(usize, usize, usize)>,
}

impl Editor {
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(super) fn take(&mut self) -> String {
        self.cursor = 0;
        self.selection_anchor = None;
        self.desired_column = None;
        std::mem::take(&mut self.text)
    }

    pub(super) fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.selection_anchor = None;
        self.desired_column = None;
    }

    pub(super) fn begin_selection(&mut self) {
        self.selection_anchor.get_or_insert(self.cursor);
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub(super) fn selected_text(&self) -> Option<&str> {
        self.selection_range()
            .map(|(start, end)| &self.text[start..end])
    }

    pub(super) fn collapse_selection(&mut self, right: bool) -> bool {
        let Some((start, end)) = self.selection_range() else {
            return false;
        };
        self.cursor = if right { end } else { start };
        self.selection_anchor = None;
        self.desired_column = None;
        true
    }

    fn selection_range(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        (anchor != self.cursor).then(|| (anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    pub(super) fn delete_selection(&mut self) -> bool {
        let Some((start, end)) = self.selection_range() else {
            self.selection_anchor = None;
            return false;
        };
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.selection_anchor = None;
        self.desired_column = None;
        true
    }

    pub(super) fn insert(&mut self, value: &str) {
        let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
        let safe: String = normalized
            .chars()
            .filter(|character| !character.is_control() || *character == '\n' || *character == '\t')
            .collect();
        self.delete_selection();
        self.text.insert_str(self.cursor, &safe);
        self.cursor += safe.len();
        self.desired_column = None;
    }

    pub(super) fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let previous = self.text[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map(|(index, _)| index);
        if let Some(index) = previous {
            self.text.replace_range(index..self.cursor, "");
            self.cursor = index;
            self.desired_column = None;
        }
    }

    pub(super) fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.text
                .replace_range(self.cursor..self.cursor + grapheme.len(), "");
            self.desired_column = None;
        }
    }

    pub(super) fn left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = index;
            self.desired_column = None;
        }
    }

    pub(super) fn right(&mut self) {
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
            self.desired_column = None;
        }
    }

    pub(super) fn word_left(&mut self) {
        while self.cursor > 0
            && self
                .previous_grapheme()
                .is_some_and(|grapheme| grapheme.trim().is_empty())
        {
            self.left();
        }
        while self.cursor > 0
            && self
                .previous_grapheme()
                .is_some_and(|grapheme| !grapheme.trim().is_empty())
        {
            self.left();
        }
    }

    pub(super) fn word_right(&mut self) {
        while self.cursor < self.text.len()
            && self
                .next_grapheme()
                .is_some_and(|grapheme| !grapheme.trim().is_empty())
        {
            self.right();
        }
        while self.cursor < self.text.len()
            && self
                .next_grapheme()
                .is_some_and(|grapheme| grapheme.trim().is_empty())
        {
            self.right();
        }
    }

    fn previous_grapheme(&self) -> Option<&str> {
        self.text[..self.cursor].graphemes(true).next_back()
    }
    fn next_grapheme(&self) -> Option<&str> {
        self.text[self.cursor..].graphemes(true).next()
    }

    pub(super) fn line_start(&mut self) {
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.desired_column = None;
    }
    pub(super) fn line_end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index);
        self.desired_column = None;
    }
    pub(super) fn document_start(&mut self) {
        self.cursor = 0;
        self.desired_column = None;
    }
    pub(super) fn document_end(&mut self) {
        self.cursor = self.text.len();
        self.desired_column = None;
    }

    pub(super) fn up(&mut self, width: usize) {
        self.move_vertical(width, false);
    }
    pub(super) fn down(&mut self, width: usize) {
        self.move_vertical(width, true);
    }

    fn move_vertical(&mut self, width: usize, down: bool) {
        let layout = self.layout(width);
        let target = if down {
            layout.cursor_row + 1
        } else {
            let Some(previous) = layout.cursor_row.checked_sub(1) else {
                return;
            };
            previous
        };
        let desired = self.desired_column.unwrap_or(layout.cursor_column);
        if let Some((index, _, _)) = layout
            .boundaries
            .iter()
            .filter(|(_, row, _)| *row == target)
            .min_by_key(|(_, _, column)| column.abs_diff(desired))
        {
            self.cursor = *index;
            self.desired_column = Some(desired);
        }
    }

    pub(super) fn delete_word_left(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.cursor;
        self.word_left();
        self.text.replace_range(self.cursor..end, "");
    }

    pub(super) fn delete_word_right(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.cursor;
        self.word_right();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub(super) fn delete_to_line_start(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.cursor;
        self.line_start();
        self.text.replace_range(self.cursor..end, "");
    }

    pub(super) fn delete_to_line_end(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.cursor;
        self.line_end();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub(super) fn layout(&self, width: usize) -> Layout {
        let width = width.max(1);
        let mut lines = vec![String::new()];
        let mut segments = vec![Vec::new()];
        let selection = self.selection_range();
        let mut row = 0;
        let mut column = 0;
        let mut boundaries = Vec::new();
        for (index, grapheme) in self.text.grapheme_indices(true) {
            if grapheme == "\n" {
                boundaries.push((index, row, column));
                lines.push(String::new());
                segments.push(Vec::new());
                row += 1;
                column = 0;
                continue;
            }
            let displayed = if grapheme == "\t" { "    " } else { grapheme };
            let cells = displayed.width().max(1);
            if column + cells > width && column > 0 {
                lines.push(String::new());
                segments.push(Vec::new());
                row += 1;
                column = 0;
            }
            boundaries.push((index, row, column));
            lines[row].push_str(displayed);
            segments[row].push(Segment {
                text: displayed.to_owned(),
                selected: selection.is_some_and(|(start, end)| index >= start && index < end),
            });
            column += cells;
        }
        boundaries.push((self.text.len(), row, column));
        let (_, cursor_row, cursor_column) = boundaries
            .iter()
            .find(|(index, _, _)| *index == self.cursor)
            .copied()
            .unwrap_or((self.text.len(), row, column));
        Layout {
            lines,
            segments,
            cursor_row,
            cursor_column,
            boundaries,
        }
    }
}

impl Editor {
    /// Applies a key that only edits the draft. Anything else is `Ignored`.
    pub(super) fn handle(&mut self, key: KeyEvent, width: usize) -> Edit {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let horizontal = matches!(key.code, KeyCode::Left | KeyCode::Right);
        let movement = horizontal
            || matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End
            );
        if horizontal
            && !shift
            && !control
            && !alt
            && self.collapse_selection(key.code == KeyCode::Right)
        {
            return Edit::Consumed;
        }
        if movement {
            if shift {
                self.begin_selection();
            } else {
                self.clear_selection();
            }
        }
        match (key.code, control, alt) {
            (KeyCode::Char('v'), true, _) => return self.paste(),
            (KeyCode::Char('x'), true, _) => return self.cut(),
            (KeyCode::Tab, _, _) => self.insert("    "),
            (KeyCode::Char('j'), true, _) => self.insert("\n"),
            (KeyCode::Char('a'), true, _) => {
                self.clear_selection();
                self.line_start();
            }
            (KeyCode::Char('e'), true, _) => {
                self.clear_selection();
                self.line_end();
            }
            (KeyCode::Char('b'), true, _) => {
                self.clear_selection();
                self.left();
            }
            (KeyCode::Char('f'), true, _) => {
                self.clear_selection();
                self.right();
            }
            (KeyCode::Char('w'), true, _) => self.delete_word_left(),
            (KeyCode::Char('u'), true, _) => self.delete_to_line_start(),
            (KeyCode::Char('k'), true, _) => self.delete_to_line_end(),
            (KeyCode::Char('b'), false, true) => {
                self.clear_selection();
                self.word_left();
            }
            (KeyCode::Char('f'), false, true) => {
                self.clear_selection();
                self.word_right();
            }
            (KeyCode::Left, true, _) => self.word_left(),
            (KeyCode::Right, true, _) => self.word_right(),
            (KeyCode::Left, _, _) => self.left(),
            (KeyCode::Right, _, _) => self.right(),
            (KeyCode::Up, _, _) => self.up(width),
            (KeyCode::Down, _, _) => self.down(width),
            (KeyCode::Home, true, _) => self.document_start(),
            (KeyCode::End, true, _) => self.document_end(),
            (KeyCode::Home, _, _) => self.line_start(),
            (KeyCode::End, _, _) => self.line_end(),
            (KeyCode::Backspace, true, _) | (KeyCode::Backspace, _, true) => {
                self.delete_word_left()
            }
            (KeyCode::Delete, true, _) | (KeyCode::Delete, _, true) => self.delete_word_right(),
            (KeyCode::Backspace, _, _) => self.backspace(),
            (KeyCode::Delete, _, _) => self.delete(),
            (KeyCode::Char(character), false, false) => self.insert(&character.to_string()),
            _ => return Edit::Ignored,
        }
        Edit::Consumed
    }

    fn paste(&mut self) -> Edit {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
            Ok(text) => {
                self.insert(&text);
                Edit::Consumed
            }
            Err(error) => Edit::Notice(format!("Clipboard unavailable: {error}")),
        }
    }

    fn cut(&mut self) -> Edit {
        let Some(selected) = self.selected_text().map(str::to_owned) else {
            return Edit::Consumed;
        };
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(selected)) {
            Ok(()) => {
                self.delete_selection();
                Edit::Consumed
            }
            Err(error) => Edit::Notice(format!("Clipboard unavailable: {error}")),
        }
    }

    /// Copies the selection to the clipboard. `None` when nothing is selected.
    pub(super) fn copy_selection(&self) -> Option<Result<(), String>> {
        let selected = self.selected_text()?.to_owned();
        Some(
            arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.set_text(selected))
                .map_err(|error| format!("Clipboard unavailable: {error}")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Edit, Editor};

    #[test]
    fn edits_graphemes_without_splitting_unicode() {
        let mut editor = Editor::default();
        editor.insert("a👩‍💻b");
        editor.left();
        editor.backspace();
        assert_eq!(editor.text(), "ab");
        editor.insert("界");
        assert_eq!(editor.text(), "a界b");
        assert_eq!(editor.layout(3).lines, vec!["a界", "b"]);
    }

    #[test]
    fn paste_keeps_newlines_and_never_submits() {
        let mut editor = Editor::default();
        editor.insert("first\r\nsecond\u{1b}[31m");
        assert_eq!(editor.text(), "first\nsecond[31m");
        editor.up(20);
        assert_eq!(editor.layout(20).cursor_row, 0);
    }

    #[test]
    fn arrows_follow_wrapped_rows() {
        let mut editor = Editor::default();
        editor.insert("abcdefghij");
        assert_eq!(editor.layout(4).cursor_row, 2);
        editor.up(4);
        assert_eq!(editor.layout(4).cursor_row, 1);
        editor.insert("X");
        assert_eq!(editor.text(), "abcdefXghij");
    }

    #[test]
    fn selection_replaces_graphemes_and_word_deletion_works_both_ways() {
        let mut editor = Editor::default();
        editor.insert("one 👩‍💻 two three");
        editor.document_start();
        editor.word_right();
        editor.begin_selection();
        editor.word_right();
        assert_eq!(editor.selected_text(), Some("👩‍💻 "));
        assert!(
            editor.layout(40).segments[0]
                .iter()
                .any(|segment| segment.selected)
        );
        editor.insert("x ");
        assert_eq!(editor.text(), "one x two three");
        editor.delete_word_right();
        assert_eq!(editor.text(), "one x three");
        editor.delete_word_left();
        assert_eq!(editor.text(), "one three");
    }

    #[test]
    fn handle_consumes_editing_keys_and_ignores_the_rest() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut editor = Editor::default();
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        assert!(matches!(
            editor.handle(key(KeyCode::Char('h'), KeyModifiers::NONE), 40),
            Edit::Consumed
        ));
        assert!(matches!(
            editor.handle(key(KeyCode::Char('i'), KeyModifiers::SHIFT), 40),
            Edit::Consumed
        ));
        assert_eq!(editor.text(), "hi");
        assert!(matches!(
            editor.handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL), 40),
            Edit::Consumed
        ));
        assert!(matches!(
            editor.handle(key(KeyCode::Right, KeyModifiers::SHIFT), 40),
            Edit::Consumed
        ));
        assert_eq!(editor.selected_text(), Some("h"));
        assert!(matches!(
            editor.handle(key(KeyCode::Enter, KeyModifiers::NONE), 40),
            Edit::Ignored
        ));
        assert!(matches!(
            editor.handle(key(KeyCode::F(5), KeyModifiers::NONE), 40),
            Edit::Ignored
        ));
        assert!(matches!(
            editor.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL), 40),
            Edit::Ignored
        ));
        assert!(matches!(
            editor.handle(key(KeyCode::Tab, KeyModifiers::NONE), 40),
            Edit::Consumed
        ));
        assert_eq!(editor.text(), "    i");
    }
}

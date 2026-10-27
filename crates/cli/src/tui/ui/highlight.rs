//! Syntax highlighting for code in the chat, colored from the terminal's own
//! 16-color palette so it reads on light and dark themes alike.

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};

type Lines = Vec<Vec<Span<'static>>>;

/// How many highlighted blocks to keep, so redrawing a long reply every
/// tick does not highlight the same code again.
const MEMO_SIZE: usize = 64;
/// Blocks past these limits render plain: highlighting them on every redraw
/// would stall the chat, and every line is cut to the terminal width anyway.
const MAX_BYTES: usize = 64 * 1024;
const MAX_LINES: usize = 2_000;
const MAX_LINE_BYTES: usize = 1_024;

/// Highlighted blocks, most recently used last.
#[derive(Default)]
struct Memo {
    lines: HashMap<u64, Lines>,
    order: VecDeque<u64>,
}

impl Memo {
    fn get(&mut self, key: u64) -> Option<Lines> {
        let lines = self.lines.get(&key)?.clone();
        self.order.retain(|k| *k != key);
        self.order.push_back(key);
        Some(lines)
    }

    fn put(&mut self, key: u64, lines: Lines) {
        if self.lines.len() >= MEMO_SIZE
            && let Some(oldest) = self.order.pop_front()
        {
            self.lines.remove(&oldest);
        }
        self.lines.insert(key, lines);
        self.order.push_back(key);
    }
}

#[cfg(test)]
thread_local! {
    /// Highlighting work done on this thread, so a test can see a memo hit
    /// without other tests running in parallel counting too.
    pub(super) static MISSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        two_face::theme::extra()
            .get(two_face::theme::EmbeddedThemeName::Ansi)
            .clone()
    })
}

fn memo() -> &'static Mutex<Memo> {
    static MEMO: OnceLock<Mutex<Memo>> = OnceLock::new();
    MEMO.get_or_init(Mutex::default)
}

fn syntax_for(language: &str) -> Option<&'static SyntaxReference> {
    if language.is_empty() {
        return None;
    }
    let set = syntaxes();
    set.find_syntax_by_token(language)
        .or_else(|| set.find_syntax_by_extension(language))
        .or_else(|| set.find_syntax_by_name(language))
}

/// One line of spans per line of `code`, or `None` when the language is
/// unknown so the caller shows plain text. `language` is a fence tag
/// (`rust`, `py`) or a file extension (`rs`, `toml`). The text of each line
/// is unchanged; only its styling is added.
pub(super) fn highlight(code: &str, language: &str) -> Option<Lines> {
    if code.len() > MAX_BYTES
        || code.lines().count() > MAX_LINES
        || code.lines().any(|line| line.len() > MAX_LINE_BYTES)
    {
        return None;
    }
    let syntax = syntax_for(language)?;
    let mut hasher = DefaultHasher::new();
    (language, code).hash(&mut hasher);
    let key = hasher.finish();
    if let Some(lines) = memo().lock().ok()?.get(key) {
        return Some(lines);
    }
    #[cfg(test)]
    MISSES.with(|misses| misses.set(misses.get() + 1));
    let mut highlighter = HighlightLines::new(syntax, theme());
    let mut lines = Vec::new();
    for line in code.split('\n') {
        // syntect wants each line with its newline, but the spans should not
        // carry it.
        let with_newline = format!("{line}\n");
        let ranges = highlighter.highlight_line(&with_newline, syntaxes()).ok()?;
        let spans = ranges
            .into_iter()
            .filter_map(|(style, text)| {
                let text = text.strip_suffix('\n').unwrap_or(text);
                (!text.is_empty()).then(|| Span::styled(text.to_owned(), terminal_style(style)))
            })
            .collect();
        lines.push(spans);
    }
    memo().lock().ok()?.put(key, lines.clone());
    Some(lines)
}

/// What to look a file's syntax up by: its extension, or its whole name when
/// it has none (`Makefile`, `Dockerfile`).
pub(super) fn language_for_path(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => name,
    }
}

/// The ANSI theme encodes a terminal palette index in `r` when alpha is 0,
/// and the terminal's default color when alpha is 1. Indexes 0 to 15 become
/// the named colors, which every terminal supports; this theme uses no others.
fn terminal_style(style: syntect::highlighting::Style) -> Style {
    let color = style.foreground;
    let mut out = Style {
        fg: (color.a == 0).then(|| palette_color(color.r)).flatten(),
        ..Style::default()
    };
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn palette_color(index: u8) -> Option<Color> {
    Some(match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn text(line: &[Span<'static>]) -> String {
        line.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn highlights_known_languages_without_changing_text() {
        let lines = highlight("fn main() {}\nlet x = 1;", "rust").unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(text(&lines[0]), "fn main() {}");
        assert_eq!(text(&lines[1]), "let x = 1;");
        assert!(lines[0].iter().any(|span| span.style.fg.is_some()));
    }

    #[test]
    fn unknown_languages_fall_back() {
        assert!(highlight("x", "nolang").is_none());
        assert!(highlight("x", "").is_none());
    }

    #[test]
    fn extensions_and_tags_both_resolve() {
        assert!(highlight("[a]\nb = 1", "toml").is_some());
        assert!(highlight("x = 1", "py").is_some());
        assert!(highlight("const x = 1", "typescript").is_some());
    }

    #[test]
    fn colors_come_from_the_terminal_palette() {
        for line in highlight("fn x() { let s = \"a\"; }", "rust").unwrap() {
            for span in line {
                match span.style.fg {
                    Some(Color::Indexed(n)) | Some(Color::Rgb(n, _, _)) => {
                        panic!("not a named palette color: {n}")
                    }
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn language_for_path_uses_the_extension() {
        assert_eq!(language_for_path("src/main.rs"), "rs");
        assert_eq!(language_for_path("Makefile"), "Makefile");
        assert_eq!(language_for_path(".bashrc"), ".bashrc");
        assert_eq!(language_for_path("a/b"), "b");
        assert_eq!(language_for_path(""), "");
    }

    #[test]
    fn repeated_calls_hit_the_memo() {
        let code = "fn memo_probe_unique() {}";
        let before = MISSES.with(|misses| misses.get());
        highlight(code, "rust");
        highlight(code, "rust");
        let after = MISSES.with(|misses| misses.get());
        assert_eq!(after - before, 1);
    }

    #[test]
    fn huge_or_minified_blocks_render_plain() {
        let long_line = "x".repeat(2_000);
        assert!(highlight(&long_line, "rust").is_none());
        let many_lines = "let x = 1;\n".repeat(3_000);
        assert!(highlight(&many_lines, "rust").is_none());
    }

    #[test]
    fn blocks_in_use_stay_memoized_while_others_come_and_go() {
        let kept = "fn kept_in_memo() {}";
        highlight(kept, "rust");
        for n in 0..(MEMO_SIZE * 2) {
            highlight(&format!("fn churn_{n}() {{}}"), "rust");
            let before = MISSES.with(|misses| misses.get());
            highlight(kept, "rust");
            assert_eq!(
                MISSES.with(|misses| misses.get()),
                before,
                "evicted after {n}"
            );
        }
    }
}

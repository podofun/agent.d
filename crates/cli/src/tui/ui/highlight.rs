//! Syntax highlighting for code in the chat, colored from the terminal's own
//! 16-color palette so it reads on light and dark themes alike.

use std::collections::HashMap;
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

fn memo() -> &'static Mutex<HashMap<u64, Lines>> {
    static MEMO: OnceLock<Mutex<HashMap<u64, Lines>>> = OnceLock::new();
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
    let syntax = syntax_for(language)?;
    let mut hasher = DefaultHasher::new();
    (language, code).hash(&mut hasher);
    let key = hasher.finish();
    if let Some(lines) = memo().lock().ok()?.get(&key) {
        return Some(lines.clone());
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
    let mut memo = memo().lock().ok()?;
    if memo.len() >= MEMO_SIZE {
        memo.clear();
    }
    memo.insert(key, lines.clone());
    Some(lines)
}

/// The ANSI theme encodes a terminal palette index in `r` when alpha is 0,
/// and the terminal's default color when alpha is 1, the way bat reads it.
fn terminal_style(style: syntect::highlighting::Style) -> Style {
    let color = style.foreground;
    let mut out = Style {
        fg: (color.a == 0 && color.r < 16).then_some(Color::Indexed(color.r)),
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

/// What to look a file's syntax up by: its extension, or its whole name when
/// it has none (`Makefile`, `Dockerfile`).
pub(super) fn language_for_path(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => extension,
        _ => name,
    }
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
                    None | Some(Color::Reset) => {}
                    Some(Color::Indexed(n)) => assert!(n < 16, "{n}"),
                    other => panic!("not a palette color: {other:?}"),
                }
            }
        }
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
    fn language_for_path_uses_the_extension() {
        assert_eq!(language_for_path("src/main.rs"), "rs");
        assert_eq!(language_for_path("Makefile"), "Makefile");
        assert_eq!(language_for_path("a/b"), "b");
        assert_eq!(language_for_path(""), "");
    }
}

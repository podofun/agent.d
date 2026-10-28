use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::super::app::{App, Request, Tab};
use super::super::presentation;
use super::super::search::{self, Row};
use super::{ERROR, MUTED, PRIMARY, SECONDARY, SUCCESS, WARNING};

const AMBER: Color = Color::Rgb(226, 192, 141);
const MINT: Color = Color::Rgb(139, 205, 164);
/// Inner width from which the preview sits beside the list instead of below it.
const SIDE_BY_SIDE: u16 = 76;
/// Fields shown first in a preview, in this order. Others follow alphabetically.
const FIRST_FIELDS: [&str; 6] = [
    "description",
    "model",
    "runner",
    "state",
    "updated_at",
    "created_at",
];
/// Bookkeeping fields a preview leaves out. `id` and `usage` are shown in their own way.
const HIDDEN_FIELDS: [&str; 6] = [
    "turn_count",
    "next_seq",
    "compactions",
    "owner",
    "id",
    "usage",
];

pub(super) fn draw_browser(frame: &mut Frame<'_>, app: &App) {
    let area = browser_area(frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(PRIMARY))
        .title(Span::styled(
            " browse ",
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        ));
    let inside = block.inner(area);
    frame.render_widget(block, area);
    let inside = Rect::new(
        inside.x + 1,
        inside.y,
        inside.width.saturating_sub(2),
        inside.height,
    );
    if inside.width < 4 || inside.height < 5 {
        return;
    }
    let matches = app.rows();
    let totals = totals(app);
    let line = |y: u16| Rect::new(inside.x, inside.y + y, inside.width, 1);
    frame.render_widget(
        Paragraph::new(query_line(app, totals[app.tab.index()], inside.width)),
        line(0),
    );
    frame.render_widget(
        Paragraph::new(tabs_line(app, &totals, inside.width)),
        line(1),
    );
    let body = Rect::new(
        inside.x,
        inside.y + 3,
        inside.width,
        inside.height.saturating_sub(4),
    );
    let selected = app.selected_index().min(matches.len().saturating_sub(1));
    match matches.get(selected) {
        Some(row) => {
            let list = List {
                matches: &matches,
                selected,
                totals: (app.tab == Tab::All).then_some(&totals),
            };
            draw_body(frame, &list, row, body);
        }
        None => {
            let message = if app.loading(Request::Lists) && app.query.is_empty() {
                "Loading…".into()
            } else if app.query.is_empty() {
                app.load_error
                    .clone()
                    .unwrap_or_else(|| "Nothing here yet. Press F5 to refresh.".into())
            } else {
                format!("Nothing matches \"{}\".", app.query)
            };
            frame.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(MUTED))
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                Rect::new(
                    body.x,
                    body.y + 1,
                    body.width,
                    body.height.saturating_sub(1),
                ),
            );
        }
    }
    let footer = line(inside.height - 1);
    frame.render_widget(
        Paragraph::new(footer_line(app, matches.get(selected), footer.width)),
        footer,
    );
}

/// How many rows match the query in each tab, indexed by `Tab::index`.
/// The All tab counts every match, not only the ones it lists.
fn totals(app: &App) -> [usize; 7] {
    let mut totals = [0; 7];
    for tab in &Tab::ALL[2..] {
        totals[tab.index()] = search::rows(app, *tab, &app.query).len();
        totals[Tab::All.index()] += totals[tab.index()];
    }
    totals
}

/// The rows a list shows. With `totals`, rows are grouped under section
/// headings that say how many of each section's matches are listed.
struct List<'a, 'r> {
    matches: &'a [Row<'r>],
    selected: usize,
    totals: Option<&'a [usize; 7]>,
}

enum ListLine {
    Heading(Tab),
    Gap,
    Row(usize),
}

impl List<'_, '_> {
    fn lines(&self) -> Vec<ListLine> {
        let mut lines = Vec::new();
        for (index, row) in self.matches.iter().enumerate() {
            let starts_section = index == 0 || self.matches[index - 1].section != row.section;
            if self.totals.is_some() && starts_section {
                if index > 0 {
                    lines.push(ListLine::Gap);
                }
                lines.push(ListLine::Heading(row.section));
            }
            lines.push(ListLine::Row(index));
        }
        lines
    }
}

/// Most of the screen, but never cramped: small terminals get all of it.
fn browser_area(screen: Rect) -> Rect {
    let width = if screen.width < 80 {
        screen.width
    } else {
        (u32::from(screen.width) * 88 / 100).clamp(80, 150) as u16
    };
    let height = if screen.height < 24 {
        screen.height
    } else {
        ((u32::from(screen.height) * 85 / 100) as u16)
            .max(24)
            .min(screen.height)
    };
    Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    )
}

/// The list and the preview: side by side when wide, stacked when narrow
/// but tall, and the list alone when there is room for nothing else.
fn draw_body(frame: &mut Frame<'_>, list: &List<'_, '_>, row: &Row<'_>, body: Rect) {
    if body.width >= SIDE_BY_SIDE {
        let natural = list.matches.iter().map(natural_width).max().unwrap_or(0) as u16;
        let widest = ((u32::from(body.width) * 55 / 100) as u16).max(body.width.saturating_sub(40));
        let list_width = natural.clamp(30, widest);
        let area_list = Rect::new(body.x, body.y, list_width, body.height);
        draw_list(frame, list, area_list);
        let preview_x = body.x + list_width + 2;
        let preview = Rect::new(preview_x, body.y, body.right() - preview_x, body.height);
        draw_preview(frame, row, preview);
    } else if body.height >= 14 {
        // The list takes what its rows need, up to all but 8 lines; the preview gets the rest.
        let list_height = (list.lines().len() as u16).clamp(1, body.height - 8);
        let area_list = Rect::new(body.x, body.y, body.width, list_height);
        draw_list(frame, list, area_list);
        let preview = Rect::new(
            body.x,
            area_list.bottom() + 1,
            body.width,
            body.bottom() - area_list.bottom() - 1,
        );
        draw_preview(frame, row, preview);
    } else {
        draw_list(frame, list, body);
    }
}

fn draw_list(frame: &mut Frame<'_>, list: &List<'_, '_>, area: Rect) {
    let height = area.height as usize;
    if height == 0 {
        return;
    }
    let lines = list.lines();
    let at = lines
        .iter()
        .position(|line| matches!(line, ListLine::Row(index) if *index == list.selected))
        .unwrap_or(0);
    let offset = (at + 1).saturating_sub(height);
    let width = area.width as usize;
    let rendered: Vec<Line> = lines
        .iter()
        .skip(offset)
        .take(height)
        .map(|line| match line {
            ListLine::Heading(section) => heading_line(list, *section, width),
            ListLine::Gap => Line::default(),
            ListLine::Row(index) => row_line(&list.matches[*index], *index == list.selected, width),
        })
        .collect();
    frame.render_widget(Paragraph::new(rendered), area);
}

/// A section's name over its rows, with "5 of 26" when the section's tab has more.
fn heading_line(list: &List<'_, '_>, section: Tab, width: usize) -> Line<'static> {
    let shown = list
        .matches
        .iter()
        .filter(|row| row.section == section)
        .count();
    let total = list.totals.map_or(shown, |totals| totals[section.index()]);
    let name = format!("  {}", tab_name(section));
    let mut spans = vec![Span::styled(
        name.clone(),
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    )];
    let more = format!("{shown} of {total}");
    if total > shown && name.width() + more.width() + 2 <= width {
        spans.push(Span::raw(" ".repeat(width - name.width() - more.width())));
        spans.push(Span::styled(
            more,
            Style::default()
                .fg(MUTED)
                .add_modifier(Modifier::ITALIC | Modifier::DIM),
        ));
    }
    Line::from(spans)
}

fn draw_preview(frame: &mut Frame<'_>, row: &Row<'_>, area: Rect) {
    if area.width < 12 || area.height < 3 {
        return;
    }
    let (symbol, color) = icon(row);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(MUTED))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(symbol, Style::default().fg(color)),
            Span::styled(
                format!(" {} ", kind(row.section)),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ),
        ]));
    frame.render_widget(
        Paragraph::new(preview_lines(row))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn query_line(app: &App, found: usize, width: u16) -> Line<'static> {
    let mut spans = vec![Span::styled(
        "› ",
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
    )];
    if app.query.is_empty() {
        spans.push(Span::styled(
            "Search everywhere…",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        ));
    } else {
        spans.push(Span::raw(app.query.clone()));
        spans.push(Span::styled("▏", Style::default().fg(PRIMARY)));
    }
    let count = format!("{found} {} ", if found == 1 { "result" } else { "results" });
    let used: usize = spans.iter().map(|span| span.content.width()).sum();
    if used + count.width() + 2 <= width as usize {
        spans.push(Span::raw(" ".repeat(width as usize - used - count.width())));
        spans.push(Span::styled(count, Style::default().fg(MUTED)));
    }
    Line::from(spans)
}

/// Every section with its match count when it fits, then without counts,
/// then only the current section with arrows to the others.
fn tabs_line(app: &App, totals: &[usize; 7], width: u16) -> Line<'static> {
    let tabs = &Tab::ALL[1..];
    let counts: Vec<usize> = tabs.iter().map(|tab| totals[tab.index()]).collect();
    let tab_spans = |with_counts: bool| {
        let mut spans = Vec::new();
        for (tab, count) in tabs.iter().zip(&counts) {
            if !spans.is_empty() {
                spans.push(Span::raw("   "));
            }
            let current = *tab == app.tab;
            let style = if current {
                Style::default()
                    .fg(PRIMARY)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                Style::default().fg(MUTED)
            };
            spans.push(Span::styled(tab_name(*tab), style));
            if with_counts {
                spans.push(Span::styled(
                    format!(" {count}"),
                    Style::default()
                        .fg(if current { PRIMARY } else { MUTED })
                        .add_modifier(Modifier::DIM),
                ));
            }
        }
        spans
    };
    for with_counts in [true, false] {
        let spans = tab_spans(with_counts);
        if spans.iter().map(|span| span.content.width()).sum::<usize>() <= width as usize {
            return Line::from(spans);
        }
    }
    let position = app.tab.index().max(1);
    Line::from(vec![
        Span::styled("‹ ", Style::default().fg(MUTED)),
        Span::styled(
            tab_name(app.tab),
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", counts[position - 1]),
            Style::default().fg(PRIMARY).add_modifier(Modifier::DIM),
        ),
        Span::styled(" › ", Style::default().fg(MUTED)),
        Span::styled(
            format!(" {position}/{}", tabs.len()),
            Style::default().fg(MUTED),
        ),
    ])
}

/// Key hints, dropped from the end until the rest fits.
fn footer_line(app: &App, selected: Option<&Row<'_>>, width: u16) -> Line<'static> {
    let mut hints = vec![("↑↓", "move"), ("←→", "section")];
    if let Some(action) = selected.and_then(|row| enter_label(row.section)) {
        hints.push(("↵", action));
    }
    hints.push((
        "esc",
        if app.query.is_empty() {
            "close"
        } else {
            "clear"
        },
    ));
    hints.push(("F5", "refresh"));
    let mut spans = Vec::new();
    let mut used = 0;
    for (key, label) in hints {
        let gap = if spans.is_empty() { 0 } else { 3 };
        let needed = gap + key.width() + 1 + label.width();
        if used + needed > width as usize {
            break;
        }
        used += needed;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(
            key,
            Style::default().fg(SECONDARY).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {label}"),
            Style::default().fg(MUTED),
        ));
    }
    Line::from(spans)
}

/// Marker, icon, the full name, then the detail and, at the right edge,
/// the aside as room allows.
fn row_line(row: &Row<'_>, selected: bool, width: usize) -> Line<'static> {
    let (symbol, color) = icon(row);
    let mut spans = vec![
        if selected {
            Span::styled(
                "› ",
                Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        },
        Span::styled(symbol, Style::default().fg(color)),
        Span::raw(" "),
    ];
    let aside_width = if row.aside.is_empty() {
        0
    } else {
        row.aside.width() + 2
    };
    let room = width.saturating_sub(4 + aside_width);
    let name = ellipsize(&row.name, room);
    let name_style = if selected {
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let mut used = 4 + name.width();
    spans.push(Span::styled(name, name_style));
    let rest = room.saturating_sub(used - 4);
    if !row.detail.is_empty() && rest >= 6 {
        let detail = ellipsize(&row.detail, rest - 2);
        used += 2 + detail.width();
        spans.push(Span::raw("  "));
        spans.push(Span::styled(detail, Style::default().fg(MUTED)));
    }
    if aside_width > 0 && used + aside_width <= width {
        spans.push(Span::raw(" ".repeat(width - used - row.aside.width())));
        spans.push(Span::styled(
            row.aside.clone(),
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// The width a row needs to show everything without shortening.
fn natural_width(row: &Row<'_>) -> usize {
    let extra = |text: &str| if text.is_empty() { 0 } else { text.width() + 2 };
    4 + row.name.width() + extra(&row.detail) + extra(&row.aside) + 1
}

/// The full name in bold, then the item's fields: short values in an
/// aligned column, lists and longer text under their own headings.
fn preview_lines(row: &Row<'_>) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            row.name.clone(),
            Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    let Value::Object(fields) = row.value else {
        lines.extend([
            Line::from(vec![
                Span::styled("Run   ", Style::default().fg(MUTED)),
                Span::raw(format!("/call {}", row.name)),
            ]),
            Line::default(),
            Line::from(Span::styled(
                "Enter puts this in the composer.",
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            )),
        ]);
        return lines;
    };
    let title_key = if row.section == Tab::Sessions {
        "label"
    } else {
        "name"
    };
    let mut keys: Vec<&String> = fields
        .keys()
        .filter(|key| {
            *key != title_key && !HIDDEN_FIELDS.contains(&key.as_str()) && !fields[*key].is_null()
        })
        .collect();
    keys.sort_by_key(|key| {
        FIRST_FIELDS
            .iter()
            .position(|first| first == key)
            .unwrap_or(FIRST_FIELDS.len())
    });
    let (scalars, blocks): (Vec<&String>, Vec<&String>) =
        keys.into_iter().partition(|key| match &fields[*key] {
            Value::String(text) => !text.contains('\n'),
            Value::Array(_) | Value::Object(_) => false,
            _ => true,
        });
    let mut entries: Vec<(String, String)> = scalars
        .iter()
        .map(|key| {
            let value = &fields[*key];
            let text = match value {
                Value::Number(number) if key.ends_with("_at") => number
                    .as_u64()
                    .map_or_else(|| number.to_string(), presentation::ago),
                _ => presentation::value(value),
            };
            (presentation::field_label(key), text)
        })
        .collect();
    if let Some(tokens) = fields.get("usage").and_then(token_summary) {
        let after_runner = scalars
            .iter()
            .position(|key| *key == "runner")
            .map_or(0, |at| at + 1);
        entries.insert(after_runner, ("Tokens".into(), tokens));
    }
    let key_width = entries
        .iter()
        .map(|(label, _)| label.width())
        .max()
        .unwrap_or(0)
        .min(16);
    let fields_start = lines.len();
    for (label, text) in entries {
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<key_width$}  "), Style::default().fg(MUTED)),
            Span::raw(text),
        ]));
    }
    for key in blocks {
        let value = &fields[key];
        if lines.len() > fields_start {
            lines.push(Line::default());
        }
        let mut heading = vec![Span::styled(
            presentation::field_label(key),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if let Value::Array(items) = value {
            heading.push(Span::styled(
                format!("  {}", items.len()),
                Style::default().fg(MUTED).add_modifier(Modifier::DIM),
            ));
        }
        lines.push(Line::from(heading));
        match value {
            Value::Array(items) if items.is_empty() => lines.push(Line::from(Span::styled(
                "  none",
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            ))),
            Value::Array(items)
                if items
                    .iter()
                    .all(|item| !item.is_object() && !item.is_array()) =>
            {
                for item in items {
                    lines.push(Line::from(vec![
                        Span::styled("  • ", Style::default().fg(MUTED)),
                        Span::raw(presentation::value(item)),
                    ]));
                }
            }
            _ => {
                for text in presentation::value(value).lines() {
                    lines.push(Line::from(Span::raw(format!("  {text}"))));
                }
            }
        }
    }
    if let Some(id) = fields.get("id").and_then(Value::as_str) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            id.to_owned(),
            Style::default().fg(MUTED).add_modifier(Modifier::DIM),
        )));
    }
    lines
}

/// "12.4k in · 3.1k out", plus cached input when there is any. `None`
/// before anything has run.
fn token_summary(usage: &Value) -> Option<String> {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let (input, output) = (count("input_tokens"), count("output_tokens"));
    let cached = count("cache_read_input_tokens");
    if input + output + cached == 0 {
        return None;
    }
    let mut summary = format!(
        "{} in · {} out",
        presentation::tokens(input),
        presentation::tokens(output)
    );
    if cached > 0 {
        summary.push_str(&format!(" · {} cached", presentation::tokens(cached)));
    }
    Some(summary)
}

fn icon(row: &Row<'_>) -> (&'static str, Color) {
    match row.section {
        Tab::Sessions => ("❝", SECONDARY),
        Tab::Runners => ("◆", PRIMARY),
        Tab::Skills => ("✦", AMBER),
        Tab::Actions => ("λ", MINT),
        Tab::Services => (
            "◉",
            match row.value["state"].as_str() {
                Some("running") => SUCCESS,
                Some("failed") => ERROR,
                Some(_) => WARNING,
                None => MUTED,
            },
        ),
        Tab::Chat | Tab::All => (" ", MUTED),
    }
}

fn kind(section: Tab) -> &'static str {
    match section {
        Tab::Runners => "runner",
        Tab::Sessions => "session",
        Tab::Actions => "action",
        Tab::Services => "service",
        Tab::Skills => "skill",
        Tab::Chat | Tab::All => "",
    }
}

fn tab_name(tab: Tab) -> &'static str {
    match tab {
        Tab::All => "All",
        Tab::Sessions => "Sessions",
        Tab::Runners => "Runners",
        Tab::Skills => "Skills",
        Tab::Actions => "Actions",
        Tab::Services => "Services",
        Tab::Chat => "",
    }
}

/// What Enter does to a row of this section, if anything.
fn enter_label(section: Tab) -> Option<&'static str> {
    match section {
        Tab::Sessions => Some("open"),
        Tab::Runners => Some("chat"),
        Tab::Skills => Some("inspect"),
        Tab::Actions => Some("call"),
        Tab::Services | Tab::Chat | Tab::All => None,
    }
}

/// `text` cut to `max` columns, ending in "…" when anything was dropped.
fn ellipsize(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let width = c.width().unwrap_or(0);
        if used + width + 1 > max {
            break;
        }
        used += width;
        out.push(c);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::super::draw;
    use super::*;
    use ratatui::Terminal;
    use serde_json::json;

    #[test]
    fn browser_details_are_human_readable() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Runners;
        app.runners = vec![json!({ "name": "review", "model": "mock/test", "skills": ["git"] })];
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("◆ runner"), "{rendered}");
        assert!(rendered.contains("Model  mock/test"), "{rendered}");
        assert!(rendered.contains("• git"), "{rendered}");
        assert!(!rendered.contains("\"name\""));
        assert!(!rendered.contains("agent.d"));
    }

    #[test]
    fn all_view_groups_rows_under_section_headings_and_shows_the_query() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::All;
        app.runners = vec![json!({ "name": "helper", "model": "m" })];
        app.actions = vec![json!("git.status")];
        app.set_query("git".into());
        let text = rows(&app, 100, 30).join("\n");
        assert!(text.contains("› git"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        let heading = lines
            .iter()
            .position(|line| line.contains("│   Actions"))
            .unwrap();
        assert!(lines[heading + 1].contains("λ git.status"), "{text}");
        let list_column = lines[heading + 1].split('│').nth(1).unwrap();
        assert!(!list_column.contains("action"), "{text}");
        assert!(!text.contains("helper"), "{text}");
    }

    fn rows(app: &App, width: u16, height: u16) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn long_names_are_shown_whole_beside_a_boxed_preview() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Runners;
        let name = "x".repeat(40);
        app.runners = vec![json!({ "name": name, "model": "deepseek/deepseek-flash" })];
        let text = rows(&app, 140, 30).join("\n");
        let row = text.lines().find(|line| line.contains("› ◆")).unwrap();
        assert!(
            row.contains(&format!("{name}  deepseek/deepseek-flash")),
            "{row}"
        );
        assert!(row.contains("╭ ◆ runner ─"), "{row}");
        assert!(text.contains(&format!("│ {name}")), "{text}");
    }

    #[test]
    fn session_preview_shows_tokens_and_hides_bookkeeping() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Sessions;
        app.sessions = vec![json!({
            "id": "2e17e033-3ad2-4077-a8ed-d5bf99bf6ff5", "label": "hello", "runner": "code_helper",
            "owner": "admin:ws", "turn_count": 10, "next_seq": 10, "compactions": 0,
            "created_at": 1, "updated_at": 1,
            "usage": { "input_tokens": 12_400, "output_tokens": 3_100, "cache_read_input_tokens": 0 },
        })];
        let text = rows(&app, 140, 30).join("\n");
        assert!(text.contains("❝ hello"), "{text}");
        assert!(text.contains("Runner      code_helper"), "{text}");
        assert!(text.contains("Tokens      12.4k in · 3.1k out"), "{text}");
        assert!(
            text.contains("2e17e033-3ad2-4077-a8ed-d5bf99bf6ff5"),
            "{text}"
        );
        for hidden in ["Turn count", "Next seq", "Compactions", "Owner", "turns"] {
            assert!(!text.contains(hidden), "{hidden}: {text}");
        }
    }

    #[test]
    fn narrow_screens_stack_the_preview_and_shorten_the_tabs() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Runners;
        app.runners = vec![json!({ "name": "code_helper", "model": "deepseek/deepseek-flash" })];
        let lines = rows(&app, 48, 30);
        let text = lines.join("\n");
        assert!(text.contains("‹ Runners 1 ›"), "{text}");
        assert!(
            lines
                .iter()
                .any(|line| line.contains("│ › Search everywhere")),
            "{text}"
        );
        let list = lines
            .iter()
            .position(|line| line.contains("› ◆ code_helper"))
            .unwrap();
        let preview = lines
            .iter()
            .position(|line| line.contains("╭ ◆ runner"))
            .unwrap();
        assert!(preview > list, "{text}");
        assert!(text.contains("Model  deepseek/deepseek-flash"), "{text}");
    }

    #[test]
    fn empty_list_message_is_centered_and_shows_refresh_errors() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.tab = Tab::Sessions;
        let line = rows(&app, 100, 30)
            .into_iter()
            .find(|line| line.contains("Nothing here yet"))
            .unwrap();
        let cells: Vec<char> = line.chars().collect();
        let left = cells.iter().position(|c| *c == '│').unwrap();
        let right = cells.iter().rposition(|c| *c == '│').unwrap();
        let inner: String = cells[left + 1..right].iter().collect();
        let leading = inner.len() - inner.trim_start().len();
        let trailing = inner.len() - inner.trim_end().len();
        assert!(leading.abs_diff(trailing) <= 1, "{line}");
        app.load_error = Some("sessions.list: daemon has no session store".into());
        let text = rows(&app, 100, 30).join("\n");
        assert!(text.contains("daemon has no session store"));
        assert!(!text.contains("Nothing here yet"));
    }
}

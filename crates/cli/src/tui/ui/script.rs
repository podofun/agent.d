//! Drawing the scripted UI: chat's own parts where the layout puts them,
//! and script elements around them. All script text is drawn as text.

use agentd_ui::{NodeId, Props, Size, Tree, parts, sanitize};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::super::app::{App, Tab};
use super::{ERROR, MUTED, PRIMARY, SECONDARY, SUCCESS, WARNING, conversation};

fn to_ui(r: Rect) -> agentd_ui::Rect {
    agentd_ui::Rect::new(r.x, r.y, r.width, r.height)
}

fn from_ui(r: agentd_ui::Rect) -> Rect {
    Rect::new(r.x, r.y, r.width, r.height)
}

/// A colour from a name, `#rrggbb`, a theme token, or `@n` for a terminal
/// palette index.
pub(in crate::tui) fn color(name: &str) -> Option<Color> {
    let name = name.trim().to_ascii_lowercase();
    if let Some(hex) = name.strip_prefix('#')
        && hex.len() == 6
    {
        let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?));
    }
    if let Some(index) = name.strip_prefix('@') {
        return index.parse().ok().map(Color::Indexed);
    }
    Some(match name.as_str() {
        "accent" | "primary" => PRIMARY,
        "secondary" => SECONDARY,
        "muted" => MUTED,
        "success" => SUCCESS,
        "warning" => WARNING,
        "error" => ERROR,
        "text" | "default" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "white" => Color::White,
        _ => return None,
    })
}

/// The text style a script asked for.
pub(in crate::tui) fn style_of(style: &Props) -> Style {
    let mut out = Style::default();
    let text = |key: &str| style.get(key).and_then(Value::as_str);
    let on = |key: &str| style.get(key).and_then(Value::as_bool) == Some(true);
    if let Some(c) = text("fg").and_then(color) {
        out = out.fg(c);
    }
    if let Some(c) = text("bg").and_then(color) {
        out = out.bg(c);
    }
    for (key, modifier) in [
        ("bold", Modifier::BOLD),
        ("italic", Modifier::ITALIC),
        ("underline", Modifier::UNDERLINED),
        ("dim", Modifier::DIM),
    ] {
        if on(key) {
            out = out.add_modifier(modifier);
        }
    }
    out
}

fn prop<'a>(props: &'a Props, key: &str) -> Option<&'a str> {
    props.get(key).and_then(Value::as_str)
}

fn shown_text(node: &agentd_ui::Node) -> String {
    sanitize(prop(&node.props, "text").unwrap_or_default())
}

fn label(node: &agentd_ui::Node) -> String {
    let label = sanitize(prop(&node.props, "label").unwrap_or_default());
    match node.kind.as_str() {
        "button" => format!("[ {label} ]"),
        "checkbox" => {
            let mark = if node.props.get("checked") == Some(&Value::Bool(true)) {
                "[x]"
            } else {
                "[ ]"
            };
            format!("{mark} {label}")
        }
        _ => label,
    }
}

/// Rows `text` takes when wrapped to `width`.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    text.split('\n')
        .map(|line| line.width().div_ceil(width).max(1) as u16)
        .sum()
}

/// The height chat's prompt needs at `width`.
fn composer_height(app: &App, width: u16) -> u16 {
    let lines = app
        .input
        .layout(width.saturating_sub(6).max(1) as usize)
        .lines
        .len();
    (lines.min(6) as u16 + 2).max(3)
}

/// Where everything goes on `screen`, parents first.
pub(in crate::tui) fn layout(app: &App, screen: Rect) -> Vec<(NodeId, Rect)> {
    let measure = |_: NodeId, node: &agentd_ui::Node, width: u16| -> Size {
        let edge = 2 * agentd_ui::border_width(node);
        let (w, h) = match node.kind.as_str() {
            "chat.header" | "chat.status" => (0, 2),
            "chat.prompt" => (0, composer_height(app, width)),
            "text" => {
                let text = shown_text(node);
                let w = text.split('\n').map(|l| l.width()).max().unwrap_or(0) as u16;
                (w, wrapped_rows(&text, width.saturating_sub(edge)))
            }
            "button" | "checkbox" => (label(node).width() as u16, 1),
            "input" => {
                let shown = prop(&node.props, "value")
                    .or(prop(&node.props, "placeholder"))
                    .unwrap_or_default();
                ((shown.width() as u16 + 2).max(12), 1)
            }
            _ => (0, 0),
        };
        Size {
            width: w + edge,
            height: h + edge,
        }
    };
    agentd_ui::layout(&app.script.tree, to_ui(screen), &measure)
        .into_iter()
        .map(|(n, r)| (n, from_ui(r)))
        .collect()
}

/// What is inside a node's border and padding.
fn inner(node: &agentd_ui::Node, rect: Rect) -> Rect {
    let pad = node
        .props
        .get("padding")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(100) as u16;
    let by = agentd_ui::border_width(node)
        + if node.kind.starts_with("chat.") {
            0
        } else {
            pad
        };
    from_ui(to_ui(rect).inset(by))
}

/// The rows of chat's parts, as `screen_rows` gives them, when a script
/// moved them. A part that is not shown gets an empty row.
pub(super) fn part_rects(app: &App, screen: Rect) -> Option<[Rect; 5]> {
    if !app.script.custom() {
        return None;
    }
    let rects = layout(app, screen);
    let find = |id: NodeId| -> Rect {
        rects
            .iter()
            .find(|(n, _)| *n == id)
            .and_then(|(n, r)| app.script.tree.get(*n).map(|node| inner(node, *r)))
            .unwrap_or_default()
    };
    let mut conversation = find(parts::CONVERSATION);
    let suggestions = if app.tab == Tab::Chat && app.approval.is_none() {
        app.suggestions().len().min(6) as u16
    } else {
        0
    };
    let menu = if suggestions > 0 {
        (suggestions + 2).min(conversation.height.saturating_sub(4))
    } else {
        0
    };
    conversation.height -= menu;
    let menu = Rect::new(
        conversation.x,
        conversation.bottom(),
        conversation.width,
        menu,
    );
    Some([
        find(parts::HEADER),
        conversation,
        menu,
        find(parts::PROMPT),
        find(parts::STATUS),
    ])
}

/// The script element under `(x, y)`, the deepest one, never chat's own
/// parts or what is drawn over them.
pub(in crate::tui) fn hit(app: &App, screen: Rect, x: u16, y: u16) -> Option<NodeId> {
    if !app.script.custom() || app.approval.is_some() || app.tab != Tab::Chat {
        return None;
    }
    layout(app, screen)
        .into_iter()
        .rev()
        .find(|(_, r)| x >= r.x && y >= r.y && x < r.right() && y < r.bottom())
        .map(|(n, _)| n)
        .filter(|n| *n >= parts::FIRST_FREE)
}

/// Inner sizes of terminal elements on `screen`.
pub(in crate::tui) fn terminals(app: &App, screen: Rect) -> Vec<(NodeId, u16, u16)> {
    if !app.script.custom() {
        return Vec::new();
    }
    layout(app, screen)
        .into_iter()
        .filter_map(|(n, r)| {
            let node = app.script.tree.get(n)?;
            (node.kind == "terminal").then(|| {
                let i = inner(node, r);
                (n, i.width, i.height)
            })
        })
        .collect()
}

/// Draw the border, title and background a node's style asks for, and
/// return what is inside.
fn decorate(frame: &mut Frame<'_>, node: &agentd_ui::Node, rect: Rect) -> Rect {
    let style = style_of(&node.style);
    let border = prop(&node.style, "border");
    let border_fg = prop(&node.style, "border_fg").and_then(color);
    let mut block = Block::default().style(Style::default().bg(style.bg.unwrap_or(Color::Reset)));
    if matches!(border, Some("line" | "rounded" | "double" | "thick")) {
        block = block.borders(Borders::ALL).border_type(match border {
            Some("rounded") => BorderType::Rounded,
            Some("double") => BorderType::Double,
            Some("thick") => BorderType::Thick,
            _ => BorderType::Plain,
        });
        block = block.border_style(Style::default().fg(border_fg.or(style.fg).unwrap_or(MUTED)));
        if let Some(title) = prop(&node.style, "title") {
            block = block.title(Span::styled(
                format!(" {} ", sanitize(title).replace('\n', " ")),
                style.add_modifier(Modifier::BOLD),
            ));
        }
    }
    if style.bg.is_some() || border.is_some() {
        frame.render_widget(block, rect);
    }
    inner(node, rect)
}

/// Draw chat with the layout scripts gave it.
pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    let screen = frame.area();
    let rows = super::screen_rows(screen, app);
    let tree = &app.script.tree;
    let rects = layout(app, screen);
    // Columns from what scripts put in the header to the right edge.
    let header_right = rects
        .iter()
        .find(|(n, _)| *n == parts::HEADER_RIGHT)
        .filter(|(_, r)| r.width > 0)
        .map_or(0, |(_, r)| screen.right().saturating_sub(r.x));
    for (id, rect) in rects {
        let Some(node) = tree.get(id) else { continue };
        let style = style_of(&node.style);
        match node.kind.as_str() {
            "chat.header" => {
                decorate(frame, node, rect);
                super::draw_header(frame, app, rows[0], header_right);
            }
            "chat.conversation" => {
                decorate(frame, node, rect);
                conversation::draw_conversation(frame, app, rows[1]);
                conversation::draw_suggestions(frame, app, rows[2]);
            }
            "chat.prompt" => {
                decorate(frame, node, rect);
                let border = prop(&node.style, "border_fg")
                    .or(prop(&node.style, "fg"))
                    .and_then(color);
                super::draw_composer(frame, app, rows[3], border);
            }
            "chat.status" => {
                decorate(frame, node, rect);
                super::draw_status(frame, app, rows[4]);
            }
            "box" | "list" | "spacer" => {
                decorate(frame, node, rect);
            }
            "text" => {
                let area = decorate(frame, node, rect);
                frame.render_widget(
                    Paragraph::new(shown_text(node))
                        .style(style)
                        .wrap(Wrap { trim: false }),
                    area,
                );
            }
            "button" | "checkbox" => {
                let area = decorate(frame, node, rect);
                let mut style = style;
                if app.script.focus == Some(id) {
                    style = style.add_modifier(Modifier::REVERSED);
                } else if app.script.hover == Some(id) {
                    style = style.add_modifier(Modifier::BOLD);
                }
                frame.render_widget(Paragraph::new(label(node)).style(style), area);
            }
            "input" => {
                let area = decorate(frame, node, rect);
                let value = sanitize(prop(&node.props, "value").unwrap_or_default());
                let line = if value.is_empty() {
                    Line::styled(
                        sanitize(prop(&node.props, "placeholder").unwrap_or_default()),
                        Style::default().fg(MUTED),
                    )
                } else {
                    Line::styled(value.clone(), style)
                };
                frame.render_widget(
                    Paragraph::new(line).style(Style::default().add_modifier(Modifier::UNDERLINED)),
                    area,
                );
                if app.script.focus == Some(id) && area.width > 0 && area.height > 0 {
                    let x = area.x + (value.width() as u16).min(area.width - 1);
                    frame.set_cursor_position((x, area.y));
                }
            }
            "terminal" => {
                let area = decorate(frame, node, rect);
                draw_terminal(frame, app, id, area);
            }
            _ => {}
        }
    }
    // The selected item of each list stands out.
    for (id, node) in tree.reachable() {
        if node.kind != "list" {
            continue;
        }
        let Some(selected) = node.props.get("selected").and_then(Value::as_u64) else {
            continue;
        };
        let child = node
            .children
            .get((selected as usize).saturating_sub(1))
            .copied();
        if let Some((_, rect)) = layout(app, screen)
            .into_iter()
            .find(|(n, _)| Some(*n) == child)
        {
            let highlight = if app.script.focus == Some(id) {
                Modifier::REVERSED
            } else {
                Modifier::BOLD
            };
            frame
                .buffer_mut()
                .set_style(rect, Style::default().add_modifier(highlight));
        }
    }
}

fn draw_terminal(frame: &mut Frame<'_>, app: &App, id: NodeId, area: Rect) {
    let Some(screen) = app.script.tree.screens.get(&id) else {
        frame.render_widget(
            Paragraph::new("starting…").style(Style::default().fg(MUTED)),
            area,
        );
        return;
    };
    let lines: Vec<Line<'static>> = screen
        .lines
        .iter()
        .map(|runs| {
            Line::from(
                runs.iter()
                    .map(|run| {
                        let mut style = Style::default();
                        if let Some(c) = run.fg.as_deref().and_then(color) {
                            style = style.fg(c);
                        }
                        if let Some(c) = run.bg.as_deref().and_then(color) {
                            style = style.bg(c);
                        }
                        for (on, modifier) in [
                            (run.bold, Modifier::BOLD),
                            (run.italic, Modifier::ITALIC),
                            (run.underline, Modifier::UNDERLINED),
                            (run.inverse, Modifier::REVERSED),
                        ] {
                            if on {
                                style = style.add_modifier(modifier);
                            }
                        }
                        Span::styled(sanitize(&run.text), style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
    if app.script.focus == Some(id)
        && let Some((x, y)) = screen.cursor
        && x < area.width
        && y < area.height
    {
        frame.set_cursor_position((area.x + x, area.y + y));
    }
}

/// Elements attached to a conversation entry, as lines of text.
pub(in crate::tui) fn element_lines(tree: &Tree, id: NodeId, width: usize) -> Vec<Line<'static>> {
    let Some(node) = tree.get(id) else {
        return Vec::new();
    };
    if node.props.get("visible") == Some(&Value::Bool(false)) {
        return Vec::new();
    }
    let style = style_of(&node.style);
    match node.kind.as_str() {
        "text" => super::markdown::wrap_plain(&shown_text(node), width.max(1))
            .into_iter()
            .map(|line| line.patch_style(style))
            .collect(),
        "button" | "checkbox" => vec![Line::styled(label(node), style)],
        "box" | "list" => {
            let mut lines = Vec::new();
            if let Some(title) = prop(&node.style, "title") {
                lines.push(Line::styled(
                    sanitize(title),
                    style.add_modifier(Modifier::BOLD),
                ));
            }
            if prop(&node.props, "direction") == Some("row") {
                let mut spans = Vec::new();
                for child in &node.children {
                    for line in element_lines(tree, *child, width) {
                        spans.extend(line.spans);
                        spans.push(Span::raw(" "));
                    }
                }
                lines.push(Line::from(spans).patch_style(style));
            } else {
                for child in &node.children {
                    lines.extend(element_lines(tree, *child, width));
                }
            }
            lines
        }
        _ => Vec::new(),
    }
}

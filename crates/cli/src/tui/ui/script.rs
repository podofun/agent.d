//! Drawing the scripted UI: chat's own parts where the layout puts them,
//! and script elements around them. All script text is drawn as text.
//! A container with `scroll` shows a window onto its children, moved with
//! the mouse wheel, and text in script elements selects like the
//! conversation's: by the cells on screen.

use std::collections::HashMap;

use agentd_ui::{NodeId, Props, Size, Tree, parts, sanitize};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget, Wrap};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::super::app::{App, Tab};
use super::selection::{Point, Region, Selection};
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

/// Where the script elements are on `screen`, with the scroll offsets the
/// user gave their containers.
pub(in crate::tui) struct Placed<'a> {
    tree: &'a Tree,
    rects: HashMap<NodeId, Rect>,
    scroll: &'a HashMap<NodeId, u16>,
}

impl<'a> Placed<'a> {
    pub(in crate::tui) fn new(app: &'a App, screen: Rect) -> Self {
        Self {
            tree: &app.script.tree,
            rects: layout(app, screen).into_iter().collect(),
            scroll: &app.script.scroll,
        }
    }

    fn node(&self, id: NodeId) -> Option<(&'a agentd_ui::Node, Rect)> {
        Some((self.tree.get(id)?, *self.rects.get(&id)?))
    }

    /// The window a scroll container shows its children through, or the
    /// inside of any other element.
    pub(in crate::tui) fn viewport(&self, id: NodeId) -> Option<Rect> {
        let (node, rect) = self.node(id)?;
        Some(inner(node, rect))
    }

    /// Rows a container's children take, from the top of its viewport.
    pub(in crate::tui) fn content_height(&self, id: NodeId) -> u16 {
        let Some((node, rect)) = self.node(id) else {
            return 0;
        };
        let top = inner(node, rect).y;
        node.children
            .iter()
            .filter_map(|c| self.rects.get(c))
            .map(|r| r.bottom().saturating_sub(top))
            .max()
            .unwrap_or(0)
    }

    /// The rows a container is scrolled down by, within what it holds.
    pub(in crate::tui) fn offset(&self, id: NodeId) -> u16 {
        let Some(viewport) = self.viewport(id) else {
            return 0;
        };
        let most = self.content_height(id).saturating_sub(viewport.height);
        self.scroll.get(&id).copied().unwrap_or(0).min(most)
    }

    /// The deepest element under `(x, y)`, looking through scrolled
    /// containers.
    fn hit(&self, id: NodeId, x: u16, y: u16) -> Option<NodeId> {
        let (node, rect) = self.node(id)?;
        if !rect.contains(ratatui::layout::Position::new(x, y)) {
            return None;
        }
        let mut y = y;
        if agentd_ui::scrolls(node) {
            let viewport = inner(node, rect);
            if !viewport.contains(ratatui::layout::Position::new(x, y)) {
                return Some(id);
            }
            y = y.saturating_add(self.offset(id));
        }
        node.children
            .iter()
            .rev()
            .find_map(|child| self.hit(*child, x, y))
            .or(Some(id))
    }

    /// The region text under `(x, y)` is selected in: the innermost
    /// scrolled container, or else the outermost script element.
    fn region_at(&self, id: NodeId, x: u16, y: u16, outer: Option<NodeId>) -> Option<NodeId> {
        let (node, rect) = self.node(id)?;
        if !rect.contains(ratatui::layout::Position::new(x, y)) {
            return None;
        }
        let mut y = y;
        let mut outer = outer.or((id >= parts::FIRST_FREE).then_some(id));
        if agentd_ui::scrolls(node) {
            if !inner(node, rect).contains(ratatui::layout::Position::new(x, y)) {
                return None;
            }
            y = y.saturating_add(self.offset(id));
            outer = Some(id);
        }
        node.children
            .iter()
            .rev()
            .find_map(|child| self.region_at(*child, x, y, outer))
            .or(outer)
    }

    /// The scrolled container whose viewport is under `(x, y)`, the
    /// innermost one.
    fn scroller_at(&self, id: NodeId, x: u16, y: u16) -> Option<NodeId> {
        let (node, rect) = self.node(id)?;
        if !rect.contains(ratatui::layout::Position::new(x, y)) {
            return None;
        }
        let mut y = y;
        let mut found = None;
        if agentd_ui::scrolls(node) {
            if !inner(node, rect).contains(ratatui::layout::Position::new(x, y)) {
                return None;
            }
            y = y.saturating_add(self.offset(id));
            found = Some(id);
        }
        node.children
            .iter()
            .rev()
            .find_map(|child| self.scroller_at(*child, x, y))
            .or(found)
    }

    /// Where a region's cell under `(x, y)` is, in the region's own rows.
    pub(in crate::tui) fn point_in(&self, id: NodeId, x: u16, y: u16) -> Option<Point> {
        let viewport = self.viewport(id)?;
        if !viewport.contains(ratatui::layout::Position::new(x, y)) {
            return None;
        }
        let line = usize::from(y - viewport.y) + usize::from(self.offset(id));
        (line < usize::from(self.content_height(id).max(viewport.height))).then_some(Point {
            line,
            col: usize::from(x - viewport.x),
        })
    }
}

fn script_active(app: &App) -> bool {
    app.script.custom() && app.approval.is_none() && app.tab == Tab::Chat
}

/// The script element under `(x, y)`, the deepest one, never chat's own
/// parts or what is drawn over them.
pub(in crate::tui) fn hit(app: &App, screen: Rect, x: u16, y: u16) -> Option<NodeId> {
    if !script_active(app) {
        return None;
    }
    let placed = Placed::new(app, screen);
    placed
        .hit(app.script.tree.root?, x, y)
        .filter(|n| *n >= parts::FIRST_FREE)
}

/// The script region and the cell in it under `(x, y)`, for a selection.
pub(in crate::tui) fn point_at(app: &App, screen: Rect, x: u16, y: u16) -> Option<(NodeId, Point)> {
    if !script_active(app) {
        return None;
    }
    let placed = Placed::new(app, screen);
    let region = placed.region_at(app.script.tree.root?, x, y, None)?;
    Some((region, placed.point_in(region, x, y)?))
}

/// Scroll the container under `(x, y)` by `delta` rows. False when there
/// is none, so the conversation may scroll instead.
pub(in crate::tui) fn scroll_at(app: &mut App, screen: Rect, x: u16, y: u16, delta: i32) -> bool {
    if !script_active(app) {
        return false;
    }
    let placed = Placed::new(app, screen);
    let Some(id) = app
        .script
        .tree
        .root
        .and_then(|root| placed.scroller_at(root, x, y))
    else {
        return false;
    };
    let offset = placed.offset(id);
    let most = placed
        .content_height(id)
        .saturating_sub(placed.viewport(id).map_or(0, |v| v.height));
    let next = (i32::from(offset) + delta).clamp(0, i32::from(most)) as u16;
    app.script.scroll.insert(id, next);
    true
}

/// Scroll a region by `delta` rows while a selection is dragged past it.
pub(in crate::tui) fn scroll_region(app: &mut App, screen: Rect, id: NodeId, delta: i32) {
    let placed = Placed::new(app, screen);
    if !placed.node(id).is_some_and(|(n, _)| agentd_ui::scrolls(n)) {
        return;
    }
    let most = placed
        .content_height(id)
        .saturating_sub(placed.viewport(id).map_or(0, |v| v.height));
    let next = (i32::from(placed.offset(id)) + delta).clamp(0, i32::from(most)) as u16;
    app.script.scroll.insert(id, next);
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
fn decorate(buf: &mut Buffer, node: &agentd_ui::Node, rect: Rect) -> Rect {
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
        block.render(rect, buf);
    }
    inner(node, rect)
}

/// What a frame draws into: the screen, or a buffer for the children of a
/// scrolled container, which are drawn whole and shown through a window.
enum Target<'a, 'b> {
    Screen(&'a mut Frame<'b>),
    Buffer(&'a mut Buffer),
}

impl Target<'_, '_> {
    fn buffer(&mut self) -> &mut Buffer {
        match self {
            Target::Screen(frame) => frame.buffer_mut(),
            Target::Buffer(buf) => buf,
        }
    }

    fn render<W: Widget>(&mut self, widget: W, area: Rect) {
        widget.render(area, self.buffer());
    }

    fn cursor(&mut self, x: u16, y: u16) {
        if let Target::Screen(frame) = self {
            frame.set_cursor_position((x, y));
        }
    }
}

/// Draw chat with the layout scripts gave it.
pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    let screen = frame.area();
    let rows = super::screen_rows(screen, app);
    let placed = Placed::new(app, screen);
    // Columns from what scripts put in the header to the right edge.
    let header_right = placed
        .rects
        .get(&parts::HEADER_RIGHT)
        .filter(|r| r.width > 0)
        .map_or(0, |r| screen.right().saturating_sub(r.x));
    let Some(root) = app.script.tree.root else {
        return;
    };
    let mut target = Target::Screen(frame);
    let frame = DrawFrame {
        app,
        placed: &placed,
        rows: &rows,
        header_right,
    };
    draw_node(&mut target, &frame, root);
}

struct DrawFrame<'a> {
    app: &'a App,
    placed: &'a Placed<'a>,
    rows: &'a [Rect; 5],
    header_right: u16,
}

fn draw_node(target: &mut Target<'_, '_>, frame: &DrawFrame<'_>, id: NodeId) {
    let app = frame.app;
    let Some((node, rect)) = frame.placed.node(id) else {
        return;
    };
    let style = style_of(&node.style);
    match node.kind.as_str() {
        "chat.header" => {
            decorate(target.buffer(), node, rect);
            if let Target::Screen(f) = target {
                super::draw_header(f, app, frame.rows[0], frame.header_right);
            }
            for child in &node.children {
                draw_node(target, frame, *child);
            }
        }
        "chat.conversation" => {
            decorate(target.buffer(), node, rect);
            if let Target::Screen(f) = target {
                conversation::draw_conversation(f, app, frame.rows[1]);
                conversation::draw_suggestions(f, app, frame.rows[2]);
            }
        }
        "chat.prompt" => {
            decorate(target.buffer(), node, rect);
            let border = prop(&node.style, "border_fg")
                .or(prop(&node.style, "fg"))
                .and_then(color);
            if let Target::Screen(f) = target {
                super::draw_composer(f, app, frame.rows[3], border);
            }
        }
        "chat.status" => {
            decorate(target.buffer(), node, rect);
            if let Target::Screen(f) = target {
                super::draw_status(f, app, frame.rows[4]);
            }
        }
        "box" | "list" | "spacer" => {
            let viewport = decorate(target.buffer(), node, rect);
            if agentd_ui::scrolls(node) {
                draw_scrolled(target, frame, id, node, viewport);
            } else {
                for child in &node.children {
                    draw_node(target, frame, *child);
                }
                highlight_selected_item(target.buffer(), frame, id, node);
                highlight_selection(target.buffer(), app, id, viewport);
            }
        }
        "text" => {
            let area = decorate(target.buffer(), node, rect);
            target.render(
                Paragraph::new(shown_text(node))
                    .style(style)
                    .wrap(Wrap { trim: false }),
                area,
            );
            highlight_selection(target.buffer(), app, id, area);
        }
        "button" | "checkbox" => {
            let area = decorate(target.buffer(), node, rect);
            let mut style = style;
            if app.script.focus == Some(id) {
                style = style.add_modifier(Modifier::REVERSED);
            } else if app.script.hover == Some(id) {
                style = style.add_modifier(Modifier::BOLD);
            }
            target.render(Paragraph::new(label(node)).style(style), area);
        }
        "input" => {
            let area = decorate(target.buffer(), node, rect);
            let value = sanitize(prop(&node.props, "value").unwrap_or_default());
            let line = if value.is_empty() {
                Line::styled(
                    sanitize(prop(&node.props, "placeholder").unwrap_or_default()),
                    Style::default().fg(MUTED),
                )
            } else {
                Line::styled(value.clone(), style)
            };
            target.render(
                Paragraph::new(line).style(Style::default().add_modifier(Modifier::UNDERLINED)),
                area,
            );
            if app.script.focus == Some(id) && area.width > 0 && area.height > 0 {
                let x = area.x + (value.width() as u16).min(area.width - 1);
                target.cursor(x, area.y);
            }
        }
        "terminal" => {
            let area = decorate(target.buffer(), node, rect);
            draw_terminal(target, app, id, area);
        }
        _ => {}
    }
}

/// Draw a scrolled container's children whole, then show the rows the
/// user scrolled to through its viewport.
fn draw_scrolled(
    target: &mut Target<'_, '_>,
    frame: &DrawFrame<'_>,
    id: NodeId,
    node: &agentd_ui::Node,
    viewport: Rect,
) {
    if viewport.width == 0 || viewport.height == 0 {
        return;
    }
    let height = frame.placed.content_height(id).max(viewport.height);
    let content = Rect::new(viewport.x, viewport.y, viewport.width, height);
    let mut buf = Buffer::empty(content);
    if let Some(bg) = style_of(&node.style).bg {
        buf.set_style(content, Style::default().bg(bg));
    }
    let mut inner_target = Target::Buffer(&mut buf);
    for child in &node.children {
        draw_node(&mut inner_target, frame, *child);
    }
    highlight_selected_item(&mut buf, frame, id, node);
    let offset = frame.placed.offset(id);
    highlight_selection(&mut buf, frame.app, id, content);
    let out = target.buffer();
    for row in 0..viewport.height {
        let from = viewport.y + offset + row;
        if from >= content.bottom() {
            break;
        }
        for x in viewport.x..viewport.right() {
            out[(x, viewport.y + row)] = buf[(x, from)].clone();
        }
    }
    // A thumb in the last column shows where the window is, when there is
    // more than it shows.
    if height > viewport.height {
        let span = u32::from(viewport.height);
        let thumb = (span * span / u32::from(height)).max(1) as u16;
        let top = u32::from(offset) * u32::from(viewport.height - thumb)
            / u32::from(height - viewport.height);
        let x = viewport.right() - 1;
        for row in 0..thumb {
            let cell = &mut out[(x, viewport.y + top as u16 + row)];
            cell.set_symbol("▕");
            cell.set_style(Style::default().fg(MUTED));
        }
    }
}

/// The selected item of a list stands out.
fn highlight_selected_item(
    buf: &mut Buffer,
    frame: &DrawFrame<'_>,
    id: NodeId,
    node: &agentd_ui::Node,
) {
    if node.kind != "list" {
        return;
    }
    let Some(selected) = node.props.get("selected").and_then(Value::as_u64) else {
        return;
    };
    let Some(child) = node.children.get((selected as usize).saturating_sub(1)) else {
        return;
    };
    let Some((_, rect)) = frame.placed.node(*child) else {
        return;
    };
    let highlight = if frame.app.script.focus == Some(id) {
        Modifier::REVERSED
    } else {
        Modifier::BOLD
    };
    if let Some(rect) = clip(rect, buf.area) {
        buf.set_style(rect, Style::default().add_modifier(highlight));
    }
}

fn clip(rect: Rect, within: Rect) -> Option<Rect> {
    let x = rect.x.max(within.x);
    let y = rect.y.max(within.y);
    let right = rect.right().min(within.right());
    let bottom = rect.bottom().min(within.bottom());
    (right > x && bottom > y).then(|| Rect::new(x, y, right - x, bottom - y))
}

/// Reverse the cells of the mouse selection when it is in region `id`.
/// `area` is where the region's rows are in `buf`, the first row first.
fn highlight_selection(buf: &mut Buffer, app: &App, id: NodeId, area: Rect) {
    let Some(selection) = app
        .selection
        .as_ref()
        .filter(|s| s.region == Region::Element(id) && !s.is_click())
    else {
        return;
    };
    let Some(area) = clip(area, buf.area) else {
        return;
    };
    for row in 0..area.height {
        let y = area.y + row;
        let range = selection.columns(usize::from(row));
        if range.is_empty() {
            continue;
        }
        let end = row_end(buf, area, y);
        let mut col = 0;
        let mut x = area.x;
        while x < area.right() {
            let width = buf[(x, y)].symbol().width().max(1);
            if col < end && col < range.end && col + width > range.start {
                for dx in 0..width as u16 {
                    if x + dx < area.right() {
                        let cell = &mut buf[(x + dx, y)];
                        cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                    }
                }
            }
            col += width;
            x += width as u16;
        }
    }
}

/// One past the last cell on row `y` of `area` that shows something.
fn row_end(buf: &Buffer, area: Rect, y: u16) -> usize {
    let mut end = 0;
    let mut col = 0;
    let mut x = area.x;
    while x < area.right() {
        let symbol = buf[(x, y)].symbol();
        let width = symbol.width().max(1);
        if !symbol.trim().is_empty() {
            end = col + width;
        }
        col += width;
        x += width as u16;
    }
    end
}

/// The text of a selection in a script region, as shown on screen.
pub(in crate::tui) fn selected_text(app: &App, screen: Rect, selection: &Selection) -> String {
    let Region::Element(id) = selection.region else {
        return String::new();
    };
    if !app.script.custom() {
        return String::new();
    }
    let placed = Placed::new(app, screen);
    let Some((node, _)) = placed.node(id) else {
        return String::new();
    };
    let Some(viewport) = placed.viewport(id) else {
        return String::new();
    };
    let height = placed.content_height(id).max(viewport.height);
    let content = Rect::new(viewport.x, viewport.y, viewport.width, height);
    if content.width == 0 || content.height == 0 {
        return String::new();
    }
    let mut buf = Buffer::empty(content);
    let rows = super::screen_rows(screen, app);
    let frame = DrawFrame {
        app,
        placed: &placed,
        rows: &rows,
        header_right: 0,
    };
    let mut target = Target::Buffer(&mut buf);
    if agentd_ui::scrolls(node) {
        for child in &node.children {
            draw_node(&mut target, &frame, *child);
        }
    } else {
        draw_node(&mut target, &frame, id);
    }
    let (start, end) = selection.ordered();
    let mut out = String::new();
    for line in start.line..=end.line.min(usize::from(height) - 1) {
        let y = content.y + line as u16;
        let range = selection.columns(line);
        let row_end = row_end(&buf, content, y);
        let mut col = 0;
        let mut x = content.x;
        while x < content.right() {
            let symbol = buf[(x, y)].symbol();
            let width = symbol.width().max(1);
            if col < row_end && col < range.end && col + width > range.start {
                out.push_str(symbol);
            }
            col += width;
            x += width as u16;
        }
        out.push('\n');
    }
    out.trim_end_matches('\n').to_owned()
}

fn draw_terminal(target: &mut Target<'_, '_>, app: &App, id: NodeId, area: Rect) {
    let Some(screen) = app.script.tree.screens.get(&id) else {
        target.render(
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
    target.render(Paragraph::new(lines), area);
    if app.script.focus == Some(id)
        && let Some((x, y)) = screen.cursor
        && x < area.width
        && y < area.height
    {
        target.cursor(area.x + x, area.y + y);
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

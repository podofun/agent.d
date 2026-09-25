use agentd_types::{ApprovalKind, ApprovalRequest};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::{ERROR, MUTED, PRIMARY, SUCCESS, WARNING};

pub(super) fn draw_approval(frame: &mut Frame<'_>, request: &ApprovalRequest, scroll: usize) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(76);
    let body = approval_body(request);
    let wanted_height = body.len().saturating_add(4).min(u16::MAX as usize) as u16;
    let height = wanted_height.min(area.height.saturating_sub(2));
    let popup = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .title(" Permission request ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(WARNING)),
        popup,
    );
    let content = Rect::new(
        popup.x + 2,
        popup.y + 1,
        popup.width.saturating_sub(4),
        popup.height.saturating_sub(3),
    );
    frame.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
        content,
    );
    if popup.height >= 3 {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "[o] Once",
                    Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(
                    "[f] Always",
                    Style::default().fg(WARNING).add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled(
                    "[d] Deny",
                    Style::default().fg(ERROR).add_modifier(Modifier::BOLD),
                ),
            ])),
            Rect::new(
                popup.x + 2,
                popup.bottom().saturating_sub(2),
                popup.width.saturating_sub(4),
                1,
            ),
        );
    }
}

fn approval_body(request: &ApprovalRequest) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        request.action.clone(),
        Style::default().fg(PRIMARY).add_modifier(Modifier::BOLD),
    )];
    if let Some(tool) = &request.tool {
        lines.push(Line::styled(
            format!("Tool: {tool}"),
            Style::default().fg(MUTED),
        ));
    }
    lines.push(Line::raw(""));
    match request.kind {
        ApprovalKind::MissingGrant => {
            lines.push(Line::raw("This action needs access to:"));
            for permission in &request.missing {
                lines.push(Line::styled(
                    format!("  • {permission}"),
                    Style::default().fg(WARNING),
                ));
            }
        }
        ApprovalKind::Confirm => {
            lines.push(Line::raw("This action requires your confirmation."));
            if !request.reason.is_empty() {
                lines.push(Line::raw(request.reason.clone()));
            }
        }
        ApprovalKind::RunnerAction => {
            lines.push(Line::raw("The runner wants to use this action."));
            if !request.reason.is_empty() {
                lines.push(Line::raw(request.reason.clone()));
            }
        }
    }
    if let Some(caller) = caller_summary(request) {
        lines.push(Line::raw(""));
        lines.push(Line::styled(caller, Style::default().fg(Color::Gray)));
    }
    lines
}

fn caller_summary(request: &ApprovalRequest) -> Option<String> {
    let caller = &request.caller;
    let mut parts = Vec::new();
    if let Some(runner) = &caller.runner {
        parts.push(format!("Runner {}", runner.as_str()));
    }
    if let Some(service) = &caller.service {
        parts.push(format!("Service {}", service.as_str()));
    }
    if let Some(interface) = &caller.interface
        && !matches!(interface.as_str(), "ws" | "http")
    {
        parts.push(format!("Via {}", interface.as_str()));
    }
    if let Some(user) = &caller.user {
        parts.push(format!("User {}", user.as_str()));
    }
    (!parts.is_empty()).then(|| parts.join("  ·  "))
}

#[cfg(test)]
mod tests {
    use super::super::super::app::App;
    use super::super::draw;
    use super::*;
    use ratatui::Terminal;
    use serde_json::json;

    #[test]
    fn approval_shows_permissions_and_caller_without_protocol_json() {
        let mut app = App::new("http://127.0.0.1:7777", 1000, None);
        app.approval = Some(serde_json::from_value(json!({
            "id": 7,
            "kind": "missing_grant",
            "action": "git.status",
            "tool": "git",
            "requires": ["shell.exec:git"],
            "missing": ["fs.read:/work/**", "shell.exec:git"],
            "reason": "tool `git` has not been granted",
            "caller": { "runner": "review", "interface": "ws", "session": "ws-6", "user": null, "service": null }
        })).unwrap());
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
        assert!(rendered.contains("fs.read:/work/**"));
        assert!(rendered.contains("Runner review"));
        assert!(!rendered.contains("future runs"));
        assert!(!rendered.contains("Client ws"));
        assert!(!rendered.contains("Session ws-6"));
        assert!(!rendered.contains("{\""));
        assert!(!rendered.contains("has not been granted"));
    }

    #[test]
    fn approval_omits_transport_only_caller_but_keeps_named_source() {
        let mut request: ApprovalRequest = serde_json::from_value(json!({
            "id": 8,
            "kind": "missing_grant",
            "action": "git.status",
            "tool": "git",
            "requires": ["shell.exec:git"],
            "missing": ["shell.exec:git"],
            "reason": "missing grant",
            "caller": { "interface": "ws", "session": "ws-6" }
        }))
        .unwrap();
        assert_eq!(caller_summary(&request), None);
        request.caller.interface = Some("telegram".into());
        assert_eq!(caller_summary(&request).as_deref(), Some("Via telegram"));
    }
}

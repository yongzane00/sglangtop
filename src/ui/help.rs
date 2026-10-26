//! `?`: keybinding help overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;

const BINDINGS: &[(&str, &str)] = &[
    ("q / Ctrl+C", "quit"),
    ("Tab / Shift+Tab", "next / previous view"),
    ("← / →", "previous / next view"),
    ("1", "fleet overview (endpoints + history charts)"),
    ("2..9", "endpoint tabs (Tab reaches the rest)"),
    ("j/k or ↑/↓", "select endpoint row (fleet view)"),
    ("Enter", "open selected endpoint (fleet view)"),
    ("PgUp/PgDn / wheel", "scroll the history charts"),
    ("g / G", "jump to top / last row"),
    ("s", "cycle fleet sort column"),
    ("t", "endpoint view: overview / requests / tables"),
    ("r", "force refresh now"),
    ("p", "pause display refresh (collection continues)"),
    ("+ / -", "faster / slower refresh interval"),
    ("?", "this help"),
];

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let width = 62.min(area.width.saturating_sub(2));
    let height = (BINDINGS.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines: Vec<Line> = vec![Line::default()];
    for (keycap, action) in BINDINGS {
        lines.push(Line::from(vec![
            Span::styled(format!("  {keycap:>16}  "), t.key),
            Span::styled(*action, t.text),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("  press Esc to close", t.dim)));

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(t.heading)
                .title(Span::styled(" sglangtop keys ", t.heading))
                .style(t.bg),
        ),
        popup,
    );
}

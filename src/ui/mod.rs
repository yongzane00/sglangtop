//! Rendering. Pure functions from `&App` to the frame — no state mutation.

pub mod cards;
pub mod charts;
pub mod endpoint;
pub mod fleet;
pub mod format;
pub mod help;
pub mod panels;
pub mod theme;

#[cfg(test)]
mod tests;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::{App, View};
use crate::state::Freshness;

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    // Paint the themed background across the whole terminal.
    frame.render_widget(Block::new().style(app.theme.bg), area);

    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, app, header);
    match app.view {
        View::Fleet => fleet::draw(frame, app, body),
        View::Endpoint(i) => endpoint::draw(frame, app, i, body),
    }
    draw_footer(frame, app, footer);

    if app.show_help {
        help::draw(frame, app, area);
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let mut spans: Vec<Span> = vec![
        Span::styled(" sglangtop ", t.heading),
        Span::styled("│ ", t.dim),
    ];

    // Tab strip: 1:ALL 2:<name> …
    let tab = |label: String, active: bool| -> Span<'static> {
        let style = if active { t.tab_active } else { t.tab_inactive };
        Span::styled(format!(" {label} "), style)
    };
    spans.push(tab("1:ALL".into(), app.view == View::Fleet));
    for (i, e) in app.endpoints.iter().enumerate() {
        let label = if i < 8 {
            format!("{}:{}", i + 2, format::truncate(&e.name, 12))
        } else {
            format::truncate(&e.name, 12)
        };
        spans.push(tab(label, app.view == View::Endpoint(i)));
    }

    // Right side: pause / interval / recording status.
    let mut right: Vec<Span> = Vec::new();
    if app.paused {
        right.push(Span::styled(" PAUSED ", t.crit));
    }
    right.push(Span::styled(
        format!(" {} ", format::brief_duration(app.refresh_interval)),
        t.dim,
    ));
    if let Some(recorder) = &app.recorder {
        let dropped = recorder.dropped_batches();
        let label = if dropped > 0 {
            format!(" rec:{} rows ({dropped} drops) ", recorder.rows_written())
        } else {
            format!(" rec:{} rows ", recorder.rows_written())
        };
        let style = if dropped > 0 { t.warn } else { t.secondary };
        right.push(Span::styled(label, style));
    } else if app.recorder_error.is_some() {
        right.push(Span::styled(" rec:FAILED ", t.crit));
    }

    let left_line = Line::from(spans);
    let right_line = Line::from(right).right_aligned();
    frame.render_widget(Paragraph::new(left_line).style(t.bg), area);
    frame.render_widget(Paragraph::new(right_line).style(Style::default()), area);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let pairs: Vec<(&str, &str)> = match app.view {
        View::Fleet => vec![
            ("q", "quit"),
            ("Tab", "views"),
            ("j/k", "select"),
            ("Enter", "open"),
            ("PgUp/PgDn", "charts"),
            ("s", app.fleet_sort.label()),
            ("r", "refresh"),
            ("p", "pause"),
            ("+/-", "interval"),
            ("?", "help"),
        ],
        View::Endpoint(_) => vec![
            ("q", "quit"),
            ("Tab/←→", "views"),
            ("1", "fleet"),
            // Advertise what pressing `t` gives you next, not where you are.
            ("t", app.panel_mode.next_label()),
            ("r", "refresh"),
            ("p", "pause"),
            ("+/-", "interval"),
            ("?", "help"),
        ],
    };
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for (keycap, action) in pairs {
        spans.push(Span::styled(keycap, t.key));
        spans.push(Span::styled(format!(":{action} "), t.dim));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(t.bg), area);
}

/// Freshness → (symbol, style) used by several views.
pub fn freshness_badge(app: &App, freshness: Freshness, healthy: Option<bool>) -> (String, Style) {
    let t = &app.theme;
    match freshness {
        Freshness::Never => ("INIT".into(), t.dim),
        Freshness::Stale => ("STALE".into(), t.crit),
        Freshness::Fresh => match healthy {
            Some(true) | None => ("UP".into(), t.value),
            Some(false) => ("UNHLTH".into(), t.warn),
        },
    }
}

//! Rolling history charts: the grid embedded below the fleet table, plus the
//! single-endpoint trend charts used by the endpoint detail view.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use std::time::Instant;

use crate::app::App;
use crate::state::{EndpointState, series_id};
use crate::ui::format;

/// The charted metrics, in display order, with how to format their values.
const CHARTS: &[(&str, &str, ValueKind)] = &[
    (series_id::RUNNING, "running requests", ValueKind::Count),
    (series_id::WAITING, "waiting requests", ValueKind::Count),
    (series_id::KV_USAGE, "KV token usage", ValueKind::Fraction),
    (series_id::PROMPT_TPS, "prompt tokens/s", ValueKind::Count),
    (
        series_id::GENERATION_TPS,
        "generation tokens/s",
        ValueKind::Count,
    ),
    (series_id::REQUEST_RATE, "completions/s", ValueKind::Count),
    (
        series_id::CACHE_HIT_RATE,
        "cache hit rate (srv win)",
        ValueKind::Fraction,
    ),
    (series_id::TTFT_P95, "TTFT p95", ValueKind::Seconds),
    (series_id::ITL_P95, "inter-token p95", ValueKind::Seconds),
    (series_id::E2E_P95, "e2e latency p95", ValueKind::Seconds),
    (series_id::ABORTS, "aborts (new)", ValueKind::Count),
    (
        series_id::RETRACTIONS,
        "retractions (new)",
        ValueKind::Count,
    ),
    (
        series_id::SPEC_ACCEPT_RATE,
        "spec accept rate",
        ValueKind::Fraction,
    ),
];

#[derive(Clone, Copy, PartialEq)]
enum ValueKind {
    Count,
    Fraction,
    Seconds,
}

impl ValueKind {
    fn fmt(self, v: Option<f64>) -> String {
        match self {
            ValueKind::Count => format::count(v),
            ValueKind::Fraction => format::percent(v),
            ValueKind::Seconds => format::seconds(v),
        }
    }
}

/// Draw the scrollable chart grid (all endpoints overlaid, one colored line
/// per endpoint/series).
pub fn draw_grid(frame: &mut Frame, app: &App, area: Rect, scroll: usize) {
    // 2-column grid of charts; each chart needs ~8 rows to be readable.
    let chart_h = 8u16;
    let cols = if area.width >= 100 { 2 } else { 1 };
    let visible_rows = (area.height / chart_h).max(1) as usize;
    let total_rows = CHARTS.len().div_ceil(cols);
    let scroll = scroll.min(total_rows.saturating_sub(visible_rows));

    for vis_row in 0..visible_rows {
        let row = vis_row + scroll;
        if row >= total_rows {
            break;
        }
        for col in 0..cols {
            let idx = row * cols + col;
            let Some(&(id, title, kind)) = CHARTS.get(idx) else {
                continue;
            };
            let w = area.width / cols as u16;
            let cell = Rect {
                x: area.x + col as u16 * w,
                y: area.y + vis_row as u16 * chart_h,
                width: w,
                height: chart_h.min(area.height - vis_row as u16 * chart_h),
            };
            draw_metric_chart(frame, app, cell, title, id, kind);
        }
    }
}

/// `(seconds relative to now, value)` chart points for one plotted line.
type LinePoints = Vec<(f64, f64)>;

fn draw_metric_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    id: &'static str,
    kind: ValueKind,
) {
    let now = Instant::now();
    let window = app.config.history_window.as_secs_f64();

    // One dataset per (endpoint, model/engine series): merging different
    // series into one line would draw a meaningless sawtooth.
    let mut plotted: Vec<(usize, LinePoints, Option<f64>)> = Vec::new();
    for (i, e) in app.endpoints.iter().enumerate() {
        for ((_key, sid), ring) in &e.history {
            if *sid != id {
                continue;
            }
            let points: LinePoints = ring
                .iter()
                .filter_map(|p| {
                    let x = -(now.saturating_duration_since(p.at).as_secs_f64());
                    (x >= -window).then_some((x, p.value))
                })
                .collect();
            if !points.is_empty() {
                plotted.push((i, points, ring.latest().map(|p| p.value)));
            }
        }
    }

    let t = &app.theme;
    // Headline value must aggregate honestly per value kind: counts sum;
    // fractions and latencies never do (a sum of percentages or p95s is
    // meaningless). Label anything that isn't a plain single value.
    let latest_values: Vec<f64> = plotted.iter().filter_map(|(_, _, l)| *l).collect();
    let headline: Vec<Span> = if latest_values.is_empty() {
        vec![Span::styled(crate::ui::format::NA, t.na)]
    } else if latest_values.len() == 1 {
        vec![Span::styled(kind.fmt(Some(latest_values[0])), t.value)]
    } else {
        match kind {
            ValueKind::Count => vec![
                Span::styled(kind.fmt(Some(latest_values.iter().sum::<f64>())), t.value),
                Span::styled(" total", t.dim),
            ],
            ValueKind::Fraction => vec![
                Span::styled(
                    kind.fmt(Some(
                        latest_values.iter().sum::<f64>() / latest_values.len() as f64,
                    )),
                    t.value,
                ),
                Span::styled(" unweighted mean", t.dim),
            ],
            ValueKind::Seconds => vec![
                Span::styled(
                    kind.fmt(Some(latest_values.iter().copied().fold(f64::MIN, f64::max))),
                    t.value,
                ),
                Span::styled(" worst", t.dim),
            ],
        }
    };

    let mut title_spans = vec![Span::styled(format!(" {title} "), t.heading)];
    title_spans.extend(headline);
    title_spans.push(Span::raw(" "));
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Line::from(title_spans));

    if plotted.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("  no data", t.na))).block(block),
            area,
        );
        return;
    }

    let mut y_max = plotted
        .iter()
        .flat_map(|(_, pts, _)| pts.iter().map(|p| p.1))
        .fold(f64::NEG_INFINITY, f64::max);
    if !y_max.is_finite() || y_max <= 0.0 {
        y_max = 1.0;
    }
    // Integer count series (requests, new errors) get integer axis bounds
    // (max+1) — a fractional headroom label like "1.1 requests" is nonsense.
    let all_integers = plotted
        .iter()
        .flat_map(|(_, pts, _)| pts.iter().map(|p| p.1))
        .all(|v| v.fract() == 0.0);
    let y_max = match kind {
        ValueKind::Fraction => 1.0f64.max(y_max),
        ValueKind::Count if all_integers => y_max + 1.0,
        _ => y_max * 1.1,
    };

    let datasets: Vec<Dataset> = plotted
        .iter()
        .map(|(i, pts, _)| {
            Dataset::default()
                .name(app.endpoints[*i].name.clone())
                .data(pts)
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(t.series_color(*i))
        })
        .collect();

    let x_axis = Axis::default()
        .bounds([-window, 0.0])
        .labels(vec![
            Span::styled(
                format!("-{}", format::brief_duration(app.config.history_window)),
                t.dim,
            ),
            Span::styled("now", t.dim),
        ])
        .style(t.dim);
    let y_axis = Axis::default()
        .bounds([0.0, y_max])
        .labels(vec![
            Span::styled("0", t.dim),
            Span::styled(kind.fmt(Some(y_max)), t.dim),
        ])
        .style(t.dim);

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(x_axis)
        .y_axis(y_axis)
        .hidden_legend_constraints((Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)));
    frame.render_widget(chart, area);
}

/// A single standalone chart for one or two series ids of ONE endpoint
/// (used by the endpoint detail view). `headline` is a pre-formatted live
/// value shown next to the title (e.g. "257.6 tokens/s"); `None` hides it.
pub fn draw_single_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    headline: Option<String>,
    ids: &[&'static str],
    endpoint: Option<&EndpointState>,
) {
    let t = &app.theme;
    let now = Instant::now();
    let window = app.config.history_window.as_secs_f64().min(120.0);

    let mut lines_data: Vec<(String, LinePoints)> = Vec::new();
    if let Some(e) = endpoint {
        for (li, id) in ids.iter().enumerate() {
            let mut pts: LinePoints = Vec::new();
            for ((_key, sid), ring) in &e.history {
                if sid != id {
                    continue;
                }
                for p in ring.iter() {
                    let x = -(now.saturating_duration_since(p.at).as_secs_f64());
                    if x >= -window {
                        pts.push((x, p.value));
                    }
                }
            }
            pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if !pts.is_empty() {
                lines_data.push((format!("{id}#{li}"), pts));
            }
        }
    }

    let mut title_spans = vec![Span::styled(format!(" {title} "), t.heading)];
    if let Some(h) = headline {
        title_spans.push(Span::styled(h, t.value));
        title_spans.push(Span::raw(" "));
    }
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Line::from(title_spans));

    if lines_data.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("  no data", t.na))).block(block),
            area,
        );
        return;
    }

    let mut y_max = lines_data
        .iter()
        .flat_map(|(_, pts)| pts.iter().map(|p| p.1))
        .fold(f64::NEG_INFINITY, f64::max);
    if !y_max.is_finite() || y_max <= 0.0 {
        y_max = 1.0;
    }
    // Integer series (request counts) get an integer bound; fractional
    // series (throughput) keep the 10% headroom.
    let all_integers = lines_data
        .iter()
        .flat_map(|(_, pts)| pts.iter().map(|p| p.1))
        .all(|v| v.fract() == 0.0);
    let y_max = if all_integers {
        y_max + 1.0
    } else {
        y_max * 1.1
    };

    let datasets: Vec<Dataset> = lines_data
        .iter()
        .enumerate()
        .map(|(i, (_name, pts))| {
            Dataset::default()
                .data(pts)
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(t.series_color(i))
        })
        .collect();

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(
            Axis::default()
                .bounds([-window, 0.0])
                .labels(vec![
                    Span::styled(format!("-{window:.0}s"), t.dim),
                    Span::styled("now", t.dim),
                ])
                .style(t.dim),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels(vec![
                    Span::styled("0", t.dim),
                    Span::styled(format::count(Some(y_max)), t.dim),
                ])
                .style(t.dim),
        );
    frame.render_widget(chart, area);
}

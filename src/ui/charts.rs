//! Rolling history charts: the grid embedded below the fleet table, one line
//! per endpoint/series so machines can be compared at a glance.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Chart, Dataset, GraphType, LegendPosition, Paragraph,
};
use std::time::{Duration, Instant};

use crate::app::App;
use crate::state::series_id;
use crate::ui::format;

/// A chart needs this many rows to be readable: two borders plus enough
/// plot rows for Braille to show a shape.
pub const MIN_CHART_H: u16 = 7;

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
pub enum ValueKind {
    Count,
    Fraction,
    Seconds,
}

impl ValueKind {
    pub fn fmt(self, v: Option<f64>) -> String {
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
    // 2-column grid of charts; each chart needs ~8 rows to be readable. The
    // visible rows stretch to share the height evenly, so a tall (portrait)
    // terminal gets taller charts rather than a blank tail.
    let cols = if area.width >= 100 { 2 } else { 1 };
    let total_rows = CHARTS.len().div_ceil(cols);
    let visible_rows = ((area.height / 8).max(1) as usize).min(total_rows);
    let chart_h = area.height / visible_rows as u16;
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

/// The y-axis labels are padded to this width so the graph area, and with
/// it the number of braille dots, is known before the data is laid out.
const Y_LABEL_W: u16 = 7;

fn draw_metric_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    id: &'static str,
    kind: ValueKind,
) {
    let now = Instant::now();
    // One scrape per braille dot, newest at the right edge, so every refresh
    // shifts every line exactly one dot. The dot count follows ratatui's
    // chart layout: the y-axis labels (padded to a fixed width, capped at a
    // third of the width) plus the axis column; there are no x-axis labels,
    // the span sits in the title instead.
    let inner_w = area.width.saturating_sub(2);
    let left_w = Y_LABEL_W.min(inner_w / 3);
    let graph_w = inner_w.saturating_sub(left_w + 1);
    let dots = usize::from(graph_w) * 2;

    // One dataset per (endpoint, model/engine series): merging different
    // series into one line would draw a meaningless sawtooth.
    let mut plotted: Vec<(usize, LinePoints, Option<f64>)> = Vec::new();
    let mut oldest: Option<Duration> = None;
    for (i, e) in app.endpoints.iter().enumerate() {
        for ((_key, sid), ring) in &e.history {
            if *sid != id {
                continue;
            }
            let samples: Vec<_> = ring.iter().collect();
            let shown = &samples[samples.len().saturating_sub(dots)..];
            if let Some(first) = shown.first() {
                let age = now.saturating_duration_since(first.at);
                oldest = Some(oldest.map_or(age, |o| o.max(age)));
            }
            let last = shown.len().saturating_sub(1) as f64;
            let points: LinePoints = shown
                .iter()
                .enumerate()
                .map(|(k, p)| (k as f64 - last, p.value))
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
    if let Some(age) = oldest {
        title_spans.push(Span::styled(
            format!(" · {}", format::brief_duration(age)),
            t.dim,
        ));
    }
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
        .bounds([-(dots.saturating_sub(1) as f64), 0.0])
        .style(t.dim);
    let y_axis = Axis::default()
        .bounds([0.0, y_max])
        .labels(vec![
            Span::styled("0", t.dim),
            Span::styled(
                format!("{:>w$}", kind.fmt(Some(y_max)), w = usize::from(Y_LABEL_W)),
                t.dim,
            ),
        ])
        .style(t.dim);

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(x_axis)
        .y_axis(y_axis)
        .hidden_legend_constraints((Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)))
        // Top left: when a tall chart has room for the legend, it covers the
        // oldest samples rather than the newest.
        .legend_position(Some(LegendPosition::TopLeft));
    frame.render_widget(chart, area);
}

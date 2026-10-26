//! Rolling history charts: the grid embedded below the fleet table, plus the
//! single-endpoint trend charts used by the endpoint detail view.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use std::time::Instant;

use crate::app::App;
use crate::metrics::normalize::SeriesKey;
use crate::state::history::RingSeries;
use crate::state::{EndpointState, series_id};
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

/// One line on a chart.
pub struct SeriesSpec {
    pub id: &'static str,
    /// Legend label; `""` hides the entry (single-line charts).
    pub label: &'static str,
}

/// What to draw in one chart cell.
pub struct ChartSpec<'a> {
    pub title: &'a str,
    /// Live value for single-line charts. Multi-line charts leave this
    /// `None`: the legend already carries each line's value, and printing it
    /// twice invites the two to disagree.
    pub headline: Option<String>,
    pub series: &'a [SeriesSpec],
    pub kind: ValueKind,
}

/// Row-major grid of chart cells, each at least [`MIN_CHART_H`] tall.
/// `Fill(1)` columns so no remainder column is left unpainted.
pub fn grid_cells(area: Rect, cols: usize, max: usize) -> Vec<Rect> {
    if cols == 0 || max == 0 || area.height < MIN_CHART_H || area.width == 0 {
        return Vec::new();
    }
    let rows = ((area.height / MIN_CHART_H) as usize).min(max.div_ceil(cols));
    if rows == 0 {
        return Vec::new();
    }
    let cell_h = area.height / rows as u16;
    let mut out = Vec::with_capacity(rows * cols);
    for r in 0..rows {
        let row = Rect {
            x: area.x,
            y: area.y + r as u16 * cell_h,
            width: area.width,
            height: cell_h,
        };
        for cell in Layout::horizontal(vec![Constraint::Fill(1); cols])
            .split(row)
            .iter()
        {
            if out.len() < max {
                out.push(*cell);
            }
        }
    }
    out
}

/// Honest aggregate of several series' latest values, with a label for how
/// it was combined. Counts sum; fractions average (and say so); latencies
/// take the worst.
fn headline_value(kind: ValueKind, latest: &[f64]) -> Option<(f64, &'static str)> {
    if latest.is_empty() {
        return None;
    }
    if latest.len() == 1 {
        return Some((latest[0], ""));
    }
    match kind {
        ValueKind::Count => Some((latest.iter().sum(), " total")),
        ValueKind::Fraction => Some((
            latest.iter().sum::<f64>() / latest.len() as f64,
            " unweighted mean",
        )),
        ValueKind::Seconds => Some((
            latest.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            " worst",
        )),
    }
}

/// Trim a title line to `width`, dropping the least important spans first.
/// Legend labels go before values, values before the title itself.
fn fit_title<'a>(mut spans: Vec<Span<'a>>, width: u16) -> Line<'a> {
    let budget = width.saturating_sub(2) as usize;
    while spans.len() > 1
        && spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum::<usize>()
            > budget
    {
        spans.pop();
    }
    Line::from(spans)
}

/// A chart of one endpoint's series. Draws one line per (series id, series
/// key) pair — merging several model/engine keys into a single line would
/// interleave their samples into a meaningless sawtooth.
pub fn draw_single_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    e: &EndpointState,
    spec: &ChartSpec<'_>,
) {
    let t = &app.theme;
    let now = Instant::now();
    // Same window as the fleet grid: two charts on one screen disagreeing
    // about their x-range is a trap for the reader.
    let window = app.config.history_window.as_secs_f64();

    // (series index, key index, points, latest value)
    let mut plots: Vec<(usize, usize, LinePoints, Option<f64>)> = Vec::new();
    for (si, series) in spec.series.iter().enumerate() {
        let mut matching: Vec<(&SeriesKey, &RingSeries)> = e
            .history
            .iter()
            .filter(|((_, sid), _)| *sid == series.id)
            .map(|((key, _), ring)| (key, ring))
            .collect();
        // Deterministic ordering, so colours/markers do not shuffle frame to
        // frame on a HashMap iteration.
        matching.sort_by(|a, b| a.0.cmp(b.0));
        for (ki, (_key, ring)) in matching.into_iter().enumerate() {
            // Rings are append-ordered, so points come out oldest-first.
            let points: LinePoints = ring
                .iter()
                .filter_map(|p| {
                    let x = -(now.saturating_duration_since(p.at).as_secs_f64());
                    (x >= -window).then_some((x, p.value))
                })
                .collect();
            if points.is_empty() {
                continue;
            }
            let latest = points.last().map(|p| p.1);
            plots.push((si, ki, points, latest));
        }
    }

    // Title: name, then either the caller's headline or a legend entry per
    // series carrying its own live value.
    let mut title_spans = vec![Span::styled(format!(" {} ", spec.title), t.heading)];
    match &spec.headline {
        Some(h) => {
            title_spans.push(Span::styled(h.clone(), t.value));
            title_spans.push(Span::raw(" "));
        }
        None => {
            for (si, series) in spec.series.iter().enumerate() {
                if series.label.is_empty() {
                    continue;
                }
                let latest: Vec<f64> = plots
                    .iter()
                    .filter(|(i, _, _, _)| *i == si)
                    .filter_map(|(_, _, _, v)| *v)
                    .collect();
                let (value, note) = match headline_value(spec.kind, &latest) {
                    Some((v, note)) => (spec.kind.fmt(Some(v)), note),
                    None => (format::NA.to_string(), ""),
                };
                title_spans.push(Span::styled(
                    format!("{} ", t.legend_glyph(si)),
                    t.series_color(si),
                ));
                title_spans.push(Span::styled(format!("{} ", series.label), t.dim));
                title_spans.push(Span::styled(format!("{value}{note} "), t.value));
            }
        }
    }
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(fit_title(title_spans, area.width));

    if plots.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled("  no data", t.na))).block(block),
            area,
        );
        return;
    }

    let mut y_max = plots
        .iter()
        .flat_map(|(_, _, pts, _)| pts.iter().map(|p| p.1))
        .fold(f64::NEG_INFINITY, f64::max);
    if !y_max.is_finite() || y_max <= 0.0 {
        y_max = 1.0;
    }
    let all_integers = plots
        .iter()
        .flat_map(|(_, _, pts, _)| pts.iter().map(|p| p.1))
        .all(|v| v.fract() == 0.0);
    let y_max = match spec.kind {
        // A fraction axis always spans a full 0..100%.
        ValueKind::Fraction => 1.0f64.max(y_max),
        // "1.1 requests" is nonsense: integer series get an integer bound.
        ValueKind::Count if all_integers => y_max + 1.0,
        _ => y_max * 1.1,
    };

    let datasets: Vec<Dataset> = plots
        .iter()
        .map(|(si, ki, pts, _)| {
            Dataset::default()
                .data(pts)
                .marker(t.series_marker(*ki))
                .graph_type(GraphType::Line)
                .style(t.series_color(*si))
        })
        .collect();

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(
            Axis::default()
                .bounds([-window, 0.0])
                .labels(vec![
                    Span::styled(
                        format!("-{}", format::brief_duration(app.config.history_window)),
                        t.dim,
                    ),
                    Span::styled("now", t.dim),
                ])
                .style(t.dim),
        )
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels(vec![
                    Span::styled("0", t.dim),
                    // Kind-aware, so a KV chart reads 100.0% and not 1.
                    Span::styled(spec.kind.fmt(Some(y_max)), t.dim),
                ])
                .style(t.dim),
        );
    frame.render_widget(chart, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(w: u16, h: u16) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        }
    }

    #[test]
    fn grid_cells_tile_the_area_without_overlap_or_overflow() {
        let a = area(150, 21);
        let cells = grid_cells(a, 3, 6);
        assert_eq!(cells.len(), 6);
        for c in &cells {
            assert!(c.x >= a.x && c.right() <= a.right(), "{c:?}");
            assert!(c.y >= a.y && c.bottom() <= a.bottom(), "{c:?}");
            assert!(c.height >= MIN_CHART_H, "{c:?}");
        }
        // Columns partition the width: no gap, no overlap.
        let row0: Vec<&Rect> = cells.iter().take(3).collect();
        assert_eq!(row0[0].x, a.x);
        assert_eq!(row0[0].right(), row0[1].x);
        assert_eq!(row0[1].right(), row0[2].x);
        assert_eq!(row0[2].right(), a.right());
    }

    #[test]
    fn grid_cells_respects_the_maximum_and_the_available_height() {
        // Only two rows fit, so a 6-chart request yields 4 cells at 2 cols.
        assert_eq!(grid_cells(area(120, 2 * MIN_CHART_H), 2, 6).len(), 4);
        // `max` caps it even when more rows would fit.
        assert_eq!(grid_cells(area(120, 10 * MIN_CHART_H), 2, 3).len(), 3);
    }

    #[test]
    fn grid_cells_degenerate_inputs_yield_nothing() {
        assert!(grid_cells(area(120, MIN_CHART_H - 1), 2, 6).is_empty());
        assert!(grid_cells(area(120, 30), 0, 6).is_empty());
        assert!(grid_cells(area(120, 30), 2, 0).is_empty());
        assert!(grid_cells(area(0, 0), 2, 6).is_empty());
    }
}

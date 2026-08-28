//! Tab 1: fleet overview — the endpoint table, the past-30-days usage bar
//! charts, and the rolling history charts (PgUp/PgDn scrolls the grid).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::state::Freshness;
use crate::storage::usage::{DayUsage, USAGE_WINDOW_DAYS};
use crate::ui::{format, freshness_badge};

/// A chart row needs this many terminal rows to be readable; below that the
/// charts section is dropped and the table gets everything.
const MIN_CHART_ROWS: u16 = 8;
/// The daily-usage bar section needs at least this many rows to read.
const MIN_USAGE_ROWS: u16 = 7;

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    // Table gets exactly what it needs (header + one row per endpoint, at
    // most half the space). Below it: daily usage bars, then the rolling
    // chart grid. Degrade order as the terminal shrinks: grid first, then
    // the usage bars, the table last.
    let table_needed = (app.endpoints.len() as u16).saturating_add(1);
    let table_h = table_needed.min((area.height / 2).max(1));
    let rest = area.height.saturating_sub(table_h);
    let usage_h = (rest / 3).clamp(MIN_USAGE_ROWS, 12);

    if rest >= usage_h + MIN_CHART_ROWS {
        let [table_area, usage_area, charts_area] = Layout::vertical([
            Constraint::Length(table_h),
            Constraint::Length(usage_h),
            Constraint::Min(0),
        ])
        .areas(area);
        draw_table(frame, app, table_area);
        draw_daily_usage(frame, app, usage_area);
        super::charts::draw_grid(frame, app, charts_area, app.fleet_chart_scroll);
    } else if rest >= MIN_USAGE_ROWS {
        let [table_area, usage_area] =
            Layout::vertical([Constraint::Length(table_h), Constraint::Min(0)]).areas(area);
        draw_table(frame, app, table_area);
        draw_daily_usage(frame, app, usage_area);
    } else {
        draw_table(frame, app, area);
    }
}

/// The past-30-days usage section: three bar charts (output tokens, input
/// tokens, requests per local day), fleet-wide, fed by the recorded history.
fn draw_daily_usage(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;

    // No data cases explain themselves instead of showing empty axes.
    let mut note = |text: String, style| {
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(t.dim)
            .title(Span::styled(" daily usage (30d) ", t.heading));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, style))).block(block),
            area,
        );
    };
    if let Some(reason) = &app.usage.disabled {
        note(format!(" usage history off — {reason}"), t.na);
        return;
    }
    let Some(data) = &app.usage.data else {
        match &app.usage.last_error {
            Some(e) => note(format!(" usage query failed: {e}"), t.crit),
            None => note(" loading usage history…".into(), t.na),
        }
        return;
    };

    type Getter = fn(&DayUsage) -> Option<f64>;
    let specs: [(&str, Getter); 3] = [
        ("output tokens/day", |d| d.generation_tokens),
        ("input tokens/day", |d| d.prompt_tokens),
        ("requests/day", |d| d.requests),
    ];

    // Three side-by-side charts when wide, stacked when narrow.
    if area.width >= 100 {
        let [a, b, c] = Layout::horizontal([Constraint::Ratio(1, 3); 3]).areas(area);
        for ((title, get), cell) in specs.into_iter().zip([a, b, c]) {
            draw_usage_chart(frame, app, cell, title, get, &data.days);
        }
    } else {
        let h = (area.height / 3).max(3);
        let [a, b, c] = Layout::vertical([
            Constraint::Length(h),
            Constraint::Length(h),
            Constraint::Min(0),
        ])
        .areas(area);
        for ((title, get), cell) in specs.into_iter().zip([a, b, c]) {
            draw_usage_chart(frame, app, cell, title, get, &data.days);
        }
    }
}

/// Geometry of the daily-usage tape. Split out from rendering so the tiling
/// invariant — the tape always spans the full inner width, with `today`
/// against the right border — is unit-testable without a terminal.
struct TapeGeometry {
    /// Days actually shown (the most recent ones when the panel is narrow).
    fit: usize,
    inner_w: usize,
    /// Separating column between bars; only afforded when slots are wide.
    gap: usize,
}

impl TapeGeometry {
    fn new(inner_w: usize, day_count: usize) -> Self {
        let fit = USAGE_WINDOW_DAYS.min(day_count).min(inner_w).max(1);
        TapeGeometry {
            fit,
            inner_w,
            gap: usize::from(inner_w / fit >= 3),
        }
    }

    /// Half-open column range owned by day `i`. Consecutive slots tile
    /// `0..inner_w` exactly; widths differ by at most one column.
    fn slot(&self, i: usize) -> (usize, usize) {
        (
            i * self.inner_w / self.fit,
            (i + 1) * self.inner_w / self.fit,
        )
    }

    /// Drawn bar width for day `i` (at least one column).
    fn bar_w(&self, i: usize) -> usize {
        let (start, end) = self.slot(i);
        (end - start).saturating_sub(self.gap).max(1)
    }

    /// Column just past the last bar — where `today` is anchored.
    fn tape_end(&self) -> usize {
        let (start, _) = self.slot(self.fit - 1);
        (start + self.bar_w(self.fit - 1)).min(self.inner_w)
    }
}

/// Hand-rendered daily-usage tape. Every slot of the window is visibly
/// accounted for — a bar (observed), a low mark (measured zero), or a faint
/// baseline dot (unobserved) — because ratatui's `BarChart` draws nothing at
/// all for zero-height bars, which made absent days look broken instead of
/// quiet. Date ticks sit at a fixed 5-day interval, anchored on today, using
/// the same relative vocabulary as the rolling charts' x-axis.
fn draw_usage_chart(
    frame: &mut Frame,
    app: &App,
    area: Rect,
    title: &str,
    get: fn(&DayUsage) -> Option<f64>,
    days: &[DayUsage],
) {
    let t = &app.theme;
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let inner_w = area.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2) as usize;
    if inner_w == 0 || inner_h < 2 {
        return;
    }
    // Last inner row is the date-tick axis; the rest is bar area.
    let chart_rows = inner_h - 1;

    // Days tile the full inner width: day `i` owns columns
    // `[i*inner_w/fit, (i+1)*inner_w/fit)`, so the tape always reaches the
    // right border and `today` sits against it. Sizing bars by a floor
    // division instead (`inner_w / 30`) quantized the tape to a multiple of
    // 30 columns, so a 59-column panel drew a 30-column tape and left half
    // its box empty while a 60-column sibling filled it — side by side they
    // read as different charts. Slot widths now differ by at most one
    // column. When even one column per day cannot fit, show only the most
    // recent days (labelled in the title).
    let geo = TapeGeometry::new(inner_w, days.len());
    let fit = geo.fit;
    let shown = &days[days.len() - fit..];

    let total: Option<f64> = shown
        .iter()
        .filter_map(get)
        .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v));
    let peak = shown.iter().filter_map(get).fold(0.0_f64, f64::max);

    let mut title_spans = vec![Span::styled(format!(" {title} "), t.heading)];
    title_spans.push(Span::styled(format!("Σ {}", format::count(total)), t.value));
    if peak > 0.0 {
        // The y-scale: bars are normalized to the busiest shown day.
        title_spans.push(Span::styled(
            format!("  pk {}", format::count(Some(peak))),
            t.dim,
        ));
    }
    if fit < USAGE_WINDOW_DAYS {
        title_spans.push(Span::styled(format!("  last {fit}d"), t.dim));
    }
    title_spans.push(Span::raw(" "));

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Per-day column height in eighth-rows; None = unobserved.
    let heights: Vec<Option<usize>> = shown
        .iter()
        .map(|d| {
            get(d).map(|v| {
                if peak <= 0.0 || v <= 0.0 {
                    0
                } else {
                    // Anything observed and non-zero shows at least ▁.
                    (((v / peak) * (chart_rows * 8) as f64).round() as usize)
                        .clamp(1, chart_rows * 8)
                }
            })
        })
        .collect();

    const EIGHTHS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let mut lines: Vec<Line> = Vec::with_capacity(chart_rows + 1);
    for row in 0..chart_rows {
        // Rows render top-down; `floor` is how many eighths sit below this row.
        let floor = (chart_rows - 1 - row) * 8;
        let mut spans: Vec<Span> = Vec::with_capacity(fit + 1);
        for (i, h) in heights.iter().enumerate() {
            let is_baseline = row == chart_rows - 1;
            let (cell, style) = match h {
                // Unobserved day: a quiet dot on the baseline, never a bar.
                None if is_baseline => (if ascii { "." } else { "·" }, t.na),
                // Measured zero: an explicit low mark, distinct from absent.
                Some(0) if is_baseline => (if ascii { "_" } else { "▁" }, t.dim),
                Some(h) if *h > floor => {
                    let filled = h - floor;
                    if filled >= 8 {
                        (if ascii { "#" } else { "█" }, t.value)
                    } else if ascii {
                        // No partial blocks in ASCII: round at half a row.
                        (if filled >= 4 { "#" } else { " " }, t.value)
                    } else {
                        (EIGHTHS[filled - 1], t.value)
                    }
                }
                _ => (" ", t.dim),
            };
            let (start, end) = geo.slot(i);
            let bar_w = geo.bar_w(i);
            spans.push(Span::styled(cell.repeat(bar_w), style));
            let pad = (end - start) - bar_w;
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
        }
        lines.push(Line::from(spans));
    }

    // Date ticks: every 5 days, anchored on today (rightmost slot), in the
    // rolling charts' relative vocabulary: `-25d … -10d -5d today`.
    // "today" is placed first and always wins; older ticks yield to it.
    let today_text = "today";
    let today_end = geo.tape_end();
    let today_start = today_end.saturating_sub(today_text.len());
    let mut axis: Vec<Span> = Vec::new();
    let mut cursor = 0usize;
    for i in 0..fit.saturating_sub(1) {
        let days_ago = fit - 1 - i;
        if !days_ago.is_multiple_of(5) {
            continue;
        }
        let text = format!("-{days_ago}d");
        let start = geo.slot(i).0.min(inner_w.saturating_sub(text.len()));
        // Skip ticks that would collide with a neighbor or with "today".
        if start < cursor || start + text.len() + 1 > today_start {
            continue;
        }
        axis.push(Span::raw(" ".repeat(start - cursor)));
        cursor = start + text.len();
        axis.push(Span::styled(text, t.dim));
    }
    if today_text.len() <= inner_w && today_start >= cursor {
        axis.push(Span::raw(" ".repeat(today_start - cursor)));
        axis.push(Span::styled(today_text, t.secondary));
    }
    lines.push(Line::from(axis));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_table(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let wide = area.width >= 110;

    let header = Row::new(
        [
            "NAME",
            "ST",
            "MODEL",
            "RUN",
            "WAIT",
            "KV",
            "PROMPT/s",
            "GEN/s",
            "REQ/s",
            // Worst p95 across the endpoint's series — never a merged
            // estimate (bucket merging across series is not assumed).
            "WORST TTFT",
            "ABRT",
            "RETR",
            "AGE",
            if wide { "GEN tokens/s TREND" } else { "" },
        ]
        .into_iter()
        .map(|h| Cell::from(Span::styled(h, t.heading))),
    );

    let mut rows: Vec<Row> = Vec::new();
    for (row_idx, &i) in app.sorted_endpoint_indices().iter().enumerate() {
        let e = &app.endpoints[i];
        let agg = e.aggregate();
        let freshness = e.freshness(now, app.refresh_interval);
        let (badge, badge_style) = freshness_badge(app, freshness, e.healthy);

        let model = if agg.models.is_empty() {
            format::NA.to_string()
        } else {
            format::truncate(&agg.models.join(","), 24)
        };

        let kv_cell = match agg.kv_usage {
            Some(kv) => {
                let v = kv.value();
                let marker = if kv.is_weighted() { "" } else { "~" };
                Span::styled(
                    format!(
                        "{}{} {}",
                        format::bar(v, 8, ascii),
                        marker,
                        format::percent(Some(v))
                    ),
                    t.by_level(v, 0.75, 0.9),
                )
            }
            None => Span::styled(format::NA.to_string(), t.na),
        };

        let err_style = if agg.abort_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.dim
        };
        let pre_style = if agg.retraction_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.dim
        };
        let wait_style = if agg.waiting.unwrap_or(0.0) > 0.0 {
            t.warn
        } else {
            t.value
        };

        // Endpoint-level generation-throughput sparkline: sum across series
        // is not directly stored, so show the first series' trend.
        let trend: String = if wide {
            e.history
                .iter()
                .find(|((_, id), _)| *id == crate::state::series_id::GENERATION_TPS)
                .map(|(_, s)| format::spark(&s.tail_values(20), 20, ascii))
                .unwrap_or_default()
        } else {
            String::new()
        };

        let age = match freshness {
            Freshness::Never => "never".to_string(),
            _ => format::ago(e.last_ok_at, now),
        };
        let age_style = if freshness == Freshness::Stale {
            t.crit
        } else {
            t.dim
        };

        let mut row = Row::new(vec![
            Cell::from(Span::styled(format::truncate(&e.name, 14), t.text)),
            Cell::from(Span::styled(badge, badge_style)),
            Cell::from(Span::styled(model, t.secondary)),
            Cell::from(Span::styled(format::count(agg.running), t.value)),
            Cell::from(Span::styled(format::count(agg.waiting), wait_style)),
            Cell::from(kv_cell),
            Cell::from(Span::styled(format::count(agg.prompt_tps), t.value)),
            Cell::from(Span::styled(format::count(agg.generation_tps), t.value)),
            Cell::from(Span::styled(format::count(agg.request_rate), t.value)),
            Cell::from(Span::styled(
                format::seconds(agg.worst_ttft_p95),
                t.secondary,
            )),
            Cell::from(Span::styled(format::count(agg.abort_delta), err_style)),
            Cell::from(Span::styled(format::count(agg.retraction_delta), pre_style)),
            Cell::from(Span::styled(age, age_style)),
            Cell::from(Span::styled(trend, t.secondary)),
        ]);
        if row_idx == app.fleet_selected {
            row = row.style(t.selected);
        }
        rows.push(row);
    }

    let widths = [
        Constraint::Length(14), // NAME
        Constraint::Length(6),  // ST
        Constraint::Length(24), // MODEL
        Constraint::Length(5),  // RUN
        Constraint::Length(5),  // WAIT
        Constraint::Length(16), // KV
        Constraint::Length(9),  // PROMPT/s
        Constraint::Length(8),  // GEN/s
        Constraint::Length(6),  // REQ/s
        Constraint::Length(10), // WORST TTFT
        Constraint::Length(4),  // ABRT
        Constraint::Length(4),  // RETR
        Constraint::Length(9),  // AGE
        Constraint::Min(0),     // TREND
    ];
    let table = Table::new(rows, widths).header(header).column_spacing(1);
    frame.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tape must span the whole panel at every width. A floor-division
    /// bar size used to quantize it to multiples of 30 columns, so sibling
    /// panels one column apart (59 vs 60, which `Ratio(1, 3)` produces
    /// routinely) drew a half-empty box next to a full one.
    #[test]
    fn tape_fills_the_panel_at_every_width() {
        for inner_w in 1..400usize {
            let geo = TapeGeometry::new(inner_w, USAGE_WINDOW_DAYS);
            // Slots tile [0, inner_w) exactly, with no gaps or overlaps.
            assert_eq!(geo.slot(0).0, 0, "inner_w={inner_w}");
            assert_eq!(geo.slot(geo.fit - 1).1, inner_w, "inner_w={inner_w}");
            for i in 1..geo.fit {
                assert_eq!(geo.slot(i - 1).1, geo.slot(i).0, "inner_w={inner_w}");
            }
            // Every day gets at least one drawn column...
            for i in 0..geo.fit {
                let (start, end) = geo.slot(i);
                assert!(end > start, "inner_w={inner_w} day={i}");
                assert!(geo.bar_w(i) >= 1, "inner_w={inner_w} day={i}");
                assert!(geo.bar_w(i) <= end - start, "inner_w={inner_w} day={i}");
            }
            // ...and widths stay even: at most one column apart.
            let widths: Vec<usize> = (0..geo.fit)
                .map(|i| geo.slot(i).1 - geo.slot(i).0)
                .collect();
            let (lo, hi) = (*widths.iter().min().unwrap(), *widths.iter().max().unwrap());
            assert!(hi - lo <= 1, "inner_w={inner_w} widths {lo}..{hi}");
            // `today` is anchored against the right border (modulo the gap).
            assert!(geo.tape_end() <= inner_w, "inner_w={inner_w}");
            assert!(
                inner_w - geo.tape_end() <= geo.gap,
                "inner_w={inner_w} leaves {} columns blank",
                inner_w - geo.tape_end()
            );
        }
    }

    /// The specific regression: two panels differing by one column must look
    /// like the same chart, not one full and one half-empty.
    #[test]
    fn sibling_panels_one_column_apart_agree() {
        let a = TapeGeometry::new(59, USAGE_WINDOW_DAYS);
        let b = TapeGeometry::new(60, USAGE_WINDOW_DAYS);
        assert_eq!(a.fit, USAGE_WINDOW_DAYS);
        assert_eq!(b.fit, USAGE_WINDOW_DAYS);
        assert_eq!(a.tape_end(), 59);
        assert_eq!(b.tape_end(), 60);
    }

    /// Panels too narrow for the whole window drop older days rather than
    /// squeezing sub-column bars; the title says so via `fit`.
    #[test]
    fn narrow_panels_show_fewer_days() {
        let geo = TapeGeometry::new(12, USAGE_WINDOW_DAYS);
        assert_eq!(geo.fit, 12);
        assert_eq!(geo.tape_end(), 12);
        let geo = TapeGeometry::new(1, USAGE_WINDOW_DAYS);
        assert_eq!(geo.fit, 1);
        assert_eq!(geo.bar_w(0), 1);
    }
}

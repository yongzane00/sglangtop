//! Token throughput: a generation band over time, and a ledger of the
//! prompts that landed.
//!
//! Generation is a steady flow, so it gets a time axis: an area of braille
//! dots under the line, one scrape per dot with the newest at the right
//! edge, so every refresh shifts the band exactly one dot, and a linear
//! scale from zero to the peak on screen. Prompts are not a flow at all.
//! SGLang counts each one whole, in one scrape interval, as its request
//! finishes (see `state::bursts`); over time they drew as a few isolated
//! bars on a scale two orders of magnitude above generation. Listed, each
//! burst is a row: how long ago the request finished, how many prompt
//! tokens it had, and how much of the context window that is.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::state::bursts::Burst;
use crate::state::requests::{RequestEntry, RequestStatus};
use crate::state::{EndpointState, series_id};
use crate::ui::format;
use crate::ui::theme::Theme;

/// Gutter: a 7-cell name/value column, a space, a 5-cell scale, the axis.
const GUTTER: u16 = 14;
const NAME_W: usize = 7;
const SCALE_W: usize = 5;
/// Narrowest plot worth drawing.
const MIN_PLOT_W: u16 = 10;
/// Braille: two dot columns per cell, four dot rows per cell row.
const DOTS_PER_CELL: usize = 2;
const DOT_ROWS: u32 = 4;
/// Braille bits by (dot column, dot row from the top).
const BRAILLE: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
/// The tailer polls the log, so a request's "Received" line can be read a
/// little late; a request finishing within the slack still belongs to
/// its burst.
const ATTRIBUTION_SLACK: Duration = Duration::from_secs(2);
/// Ledger row: age (6), gap, tokens (6), gap, then the bar and its labels.
const ROW_HEAD_W: usize = 6 + 1 + 6 + 1;
const MIN_BAR_W: usize = 8;
/// A taller band adds nothing; past this the ledger takes the height, as a
/// longer list of prompts is history worth reading.
const BAND_MAX_H: u16 = 16;
const MAX_TAIL_W: usize = 20;

/// `(seconds before now, value)`, oldest first.
type Points = Vec<(f64, f64)>;

/// The endpoint-wide series for one rate id: every model/engine series
/// summed per scrape instant. Points from one scrape share one instant, so
/// this adds engines' rates without ever mixing two scrapes into one point
/// (merging series point-by-point is what drew the old sawtooth).
pub(crate) fn endpoint_total(e: &EndpointState, id: &str, now: Instant, window: f64) -> Points {
    let mut sum: BTreeMap<Instant, f64> = BTreeMap::new();
    for ((_, sid), ring) in &e.history {
        if *sid != id {
            continue;
        }
        for p in ring.iter().filter(|p| p.value.is_finite()) {
            *sum.entry(p.at).or_insert(0.0) += p.value;
        }
    }
    sum.into_iter()
        .map(|(at, v)| (-now.saturating_duration_since(at).as_secs_f64(), v))
        .filter(|(x, _)| *x >= -window)
        .collect()
}

/// Axis labels sized for the 5-cell scale column: `64.9`, `9.7k`, `13k`.
fn axis_label(v: f64) -> String {
    if v >= 1e7 {
        format!("{:.0}M", v / 1e6)
    } else if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if v >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else if v >= 100.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Token counts for the ledger, at most 6 cells: `412`, `9.8k`, `55.2k`,
/// `1.20M`.
fn tokens_label(v: f64) -> String {
    if v >= 999_950.0 {
        format!("{:.2}M", v / 1e6)
    } else if v >= 999.95 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// One gutter row: name/value left, scale label right, then the axis glyph.
fn gutter_line(t: &Theme, left: Option<Span<'static>>, scale: Option<String>) -> Line<'static> {
    let mut spans = Vec::new();
    let left_w = match left {
        Some(s) => {
            let s = Span::styled(format::truncate(&s.content, NAME_W), s.style);
            let w = s.width();
            spans.push(s);
            w
        }
        None => 0,
    };
    spans.push(Span::raw(" ".repeat(NAME_W + 1 - left_w)));
    let axis = if scale.is_some() { "┤" } else { "│" };
    spans.push(Span::styled(
        format!("{:>width$}", scale.unwrap_or_default(), width = SCALE_W),
        t.dim,
    ));
    spans.push(Span::styled(axis, t.dim));
    Line::from(spans)
}

/// A track's gutter: `lefts` from the top (name, value, …), the max on the
/// top row and 0 on the bottom row.
fn gutter(t: &Theme, rows: u16, lefts: Vec<Span<'static>>, max: &str) -> Vec<Line<'static>> {
    let mut lefts = lefts.into_iter();
    (0..rows)
        .map(|r| {
            let scale = if r == 0 {
                Some(max.to_string())
            } else if r + 1 == rows {
                Some("0".to_string())
            } else {
                None
            };
            gutter_line(t, lefts.next(), scale)
        })
        .collect()
}

/// What a cell of the band holds, so the painter can pick a style. Ordered
/// so that the most prominent content wins when two dot columns share a
/// cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Shade {
    /// Nothing observed in either dot column.
    Empty,
    /// Only the baseline dot of a measured zero.
    Baseline,
    /// Filled dots below the crest.
    Fill,
    /// The cell holding a column's topmost dot: the line itself.
    Crest,
}

/// Fill heights in dots for the last `dots` samples, newest at the right
/// edge: `None` where no sample reaches (left of the oldest), 0 for a
/// measured zero, otherwise 1..=rows×4 against `scale`. Small is not
/// nothing: any positive rate gets at least one dot.
fn heights(values: &[f64], dots: usize, rows: u16, scale: f64) -> Vec<Option<u32>> {
    let total = u32::from(rows) * DOT_ROWS;
    let shown = &values[values.len().saturating_sub(dots)..];
    let mut out = vec![None; dots.saturating_sub(shown.len())];
    out.extend(shown.iter().map(|&v| {
        Some(if v > 0.0 && scale > 0.0 {
            ((v / scale * f64::from(total)).round() as u32).clamp(1, total)
        } else {
            0
        })
    }));
    out
}

/// Compose the band's cells, rows from the top: a glyph (`None` for an
/// empty cell) and its shade. Braille fills dot by dot; ASCII fills whole
/// cells with `#` and marks a measured zero with `_`.
fn compose(heights: &[Option<u32>], rows: u16, ascii: bool) -> Vec<Vec<(Option<char>, Shade)>> {
    let total = u32::from(rows) * DOT_ROWS;
    let cells = heights.len().div_ceil(DOTS_PER_CELL);
    let rows = usize::from(rows);
    if rows == 0 {
        return Vec::new();
    }
    let mut grid = vec![vec![(0u8, Shade::Empty); cells]; rows];
    for (d, h) in heights.iter().enumerate() {
        let Some(h) = *h else { continue };
        let (c, col) = (d / DOTS_PER_CELL, d % DOTS_PER_CELL);
        if h == 0 {
            // A measured zero keeps a baseline dot, so idle reads as alive.
            let (bits, shade) = &mut grid[rows - 1][c];
            *bits |= BRAILLE[col][3];
            *shade = (*shade).max(Shade::Baseline);
            continue;
        }
        let top = total - h;
        for dot_row in top..total {
            let (r, local) = ((dot_row / DOT_ROWS) as usize, (dot_row % DOT_ROWS) as usize);
            let (bits, shade) = &mut grid[r][c];
            *bits |= BRAILLE[col][local];
            let here = if dot_row == top {
                Shade::Crest
            } else {
                Shade::Fill
            };
            *shade = (*shade).max(here);
        }
    }
    grid.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|(bits, shade)| {
                    let glyph = match (bits, shade, ascii) {
                        (0, _, _) => None,
                        (_, Shade::Baseline, true) => Some('_'),
                        (_, _, true) => Some('#'),
                        _ => char::from_u32(0x2800 + u32::from(bits)),
                    };
                    (glyph, shade)
                })
                .collect()
        })
        .collect()
}

/// The generation band: one scrape per dot, newest at the right edge, the
/// crest a shade brighter than the fill.
fn draw_area(frame: &mut Frame, t: &Theme, area: Rect, values: &[f64], scale: f64) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let dots = usize::from(area.width) * DOTS_PER_CELL;
    let grid = compose(
        &heights(values, dots, area.height, scale),
        area.height,
        t.ascii(),
    );
    let fill = t.value.add_modifier(Modifier::DIM);
    let buf = frame.buffer_mut();
    for (r, row) in grid.iter().enumerate() {
        for (c, (glyph, shade)) in row.iter().enumerate() {
            let Some(g) = glyph else { continue };
            let style = match shade {
                Shade::Crest => t.value,
                Shade::Fill => fill,
                Shade::Baseline => t.dim,
                Shade::Empty => continue,
            };
            if let Some(cell) = buf.cell_mut((area.x + c as u16, area.y + r as u16)) {
                cell.set_char(*g).set_style(style);
            }
        }
    }
}

/// Pair bursts with the logged requests that produced them, where that is
/// certain.
///
/// `bursts` oldest first; `entries` are the log's "Received" rows in any
/// order. A request is a candidate for a burst if it was received before
/// the burst (plus the tailer's slack) and no more than a window before it.
/// Candidates are consumed in arrival order (SGLang schedules first-come
/// first-served unless told otherwise), and an id is attached only when a
/// one-prompt burst had exactly one candidate. Otherwise the row says how
/// many prompts shared it, and names nobody.
fn attribute<'a>(
    bursts: &[&Burst],
    entries: &[&'a RequestEntry],
    window: Duration,
) -> Vec<Option<&'a RequestEntry>> {
    let mut queue: Vec<&'a RequestEntry> = entries.to_vec();
    queue.sort_by_key(|e| e.seen_at);
    let mut next = 0;
    bursts
        .iter()
        .map(|b| {
            while next < queue.len() && b.at.saturating_duration_since(queue[next].seen_at) > window
            {
                next += 1;
            }
            let deadline = b.at + ATTRIBUTION_SLACK;
            let eligible = queue[next..]
                .iter()
                .take_while(|e| e.seen_at <= deadline)
                .count();
            let hit = (b.requests == Some(1) && eligible == 1).then(|| queue[next]);
            next += eligible.min(usize::try_from(b.requests.unwrap_or(1)).unwrap_or(1));
            hit
        })
        .collect()
}

/// One ledger row, ready to lay out.
struct Row<'a> {
    age: Duration,
    tokens: f64,
    /// Share of the context window, when the window is known.
    share: Option<f64>,
    /// Bar fill: the share, or the burst against the window's biggest.
    frac: f64,
    requests: Option<u32>,
    request: Option<&'a RequestEntry>,
}

/// `  -12s  55.2k ████░░░░░░ 21% ctx  chatcmpl-8e66`, exactly `width`
/// cells wide. Under pressure the request drops first, then the share,
/// before the bar shrinks below its minimum.
fn ledger_line(t: &Theme, width: usize, row: &Row) -> Line<'static> {
    let finished = matches!(
        row.request.map(|r| &r.status),
        Some(RequestStatus::Finished { .. })
    );
    let main = if finished { t.dim } else { t.secondary };
    let tail: Option<String> = match (row.request, row.requests) {
        (Some(r), _) => Some(format::truncate(&r.id, MAX_TAIL_W)),
        (None, Some(n)) if n >= 2 => Some(format!("{n} reqs")),
        _ => None,
    };
    let share: Option<String> = row.share.map(|s| {
        let pct = s * 100.0;
        let num = if pct < 0.5 {
            "<1".to_string()
        } else {
            format!("{pct:.0}")
        };
        format!(" {num:>3}% ctx")
    });
    let mut tail_w = tail.as_ref().map_or(0, |s| s.chars().count() + 2);
    let mut share_w = share.as_ref().map_or(0, |s| s.chars().count());
    let avail = width.saturating_sub(ROW_HEAD_W);
    let mut bar_w = avail.saturating_sub(tail_w + share_w);
    if bar_w < MIN_BAR_W && tail_w > 0 {
        tail_w = 0;
        bar_w = avail.saturating_sub(share_w);
    }
    if bar_w < MIN_BAR_W && share_w > 0 {
        share_w = 0;
        bar_w = avail;
    }

    let age = format!("-{}", format::brief_duration(row.age));
    let mut spans = vec![
        Span::styled(format!("{age:>6}"), t.dim),
        Span::raw(" "),
        Span::styled(format!("{:>6}", tokens_label(row.tokens)), main),
        Span::raw(" "),
    ];
    if bar_w > 0 {
        let (filled, track) = format::bar_track(row.frac, bar_w, t.ascii());
        spans.push(Span::styled(filled, main));
        spans.push(Span::styled(track, t.dim));
    }
    if share_w > 0
        && let Some(share) = share
    {
        spans.push(Span::styled(share, t.dim));
    }
    if tail_w > 0
        && let Some(tail) = tail
    {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(tail, if finished { t.dim } else { t.text }));
    }
    Line::from(spans)
}

/// Name/value rows for the ledger's left column, each truncated to fit.
fn name_column(lefts: Vec<Span<'static>>) -> Vec<Line<'static>> {
    lefts
        .into_iter()
        .map(|s| Line::from(Span::styled(format::truncate(&s.content, NAME_W), s.style)))
        .collect()
}

pub fn draw(frame: &mut Frame, t: &Theme, area: Rect, e: &EndpointState, history: Duration) {
    let window = history.as_secs_f64();
    let now = Instant::now();
    let out = endpoint_total(e, series_id::GENERATION_TPS, now, window);
    let bursts: Vec<&Burst> = e.bursts.recent(now).collect();
    let agg = e.aggregate();
    let window_label = format::brief_duration(history);

    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Span::styled(" tokens/s ", t.heading));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    if out.is_empty() && bursts.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(" no data", t.na))),
            inner,
        );
        return;
    }
    if inner.width < GUTTER + MIN_PLOT_W {
        // No room to plot: the live values alone.
        let line = Line::from(vec![
            Span::styled("out ", t.value),
            Span::styled(format::count(agg.generation_tps), t.value),
            Span::styled("  in ", t.secondary),
            Span::styled(format::count(agg.prompt_tps), t.secondary),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
        return;
    }

    // Rows: the band, its time axis when there is height to spare, and the
    // ledger, in equal halves until the band hits its cap.
    let rows = inner.height;
    let axis = u16::from(rows >= 5);
    let rest = rows - axis;
    let band_h = rest.div_ceil(2).max(2).min(rest).min(BAND_MAX_H);
    let ledger_h = rest - band_h;
    let plot_x = inner.x + GUTTER;
    let plot_w = inner.width - GUTTER;

    // Generation: the newest sample per dot, scaled to the peak on screen.
    let dots = usize::from(plot_w) * DOTS_PER_CELL;
    let shown = &out[out.len().saturating_sub(dots)..];
    let values: Vec<f64> = shown.iter().map(|p| p.1).collect();
    let peak = values.iter().copied().fold(0.0, f64::max);
    let scale = if peak > 0.0 { peak } else { 1.0 };
    let band = Rect::new(inner.x, inner.y, inner.width, band_h);
    let lefts = vec![
        Span::styled("out", t.value),
        Span::styled(format::count(agg.generation_tps), t.value),
    ];
    frame.render_widget(
        Paragraph::new(gutter(t, band_h, lefts, &axis_label(scale))),
        Rect {
            width: GUTTER,
            ..band
        },
    );
    draw_area(
        frame,
        t,
        Rect::new(plot_x, band.y, plot_w, band_h),
        &values,
        scale,
    );

    if axis == 1 {
        // The left edge is as old as the oldest sample on screen.
        let y = inner.y + band_h;
        let left = shown.first().map_or(String::new(), |(x, _)| {
            format!(
                "-{}",
                format::brief_duration(Duration::from_secs_f64((-x).max(0.0)))
            )
        });
        let pad = usize::from(plot_w).saturating_sub(left.chars().count() + 3);
        let line = Line::from(vec![
            Span::styled(left, t.dim),
            Span::raw(" ".repeat(pad)),
            Span::styled("now", t.dim),
        ]);
        frame.render_widget(Paragraph::new(line), Rect::new(plot_x, y, plot_w, 1));
    }

    if ledger_h == 0 {
        return;
    }

    // Prompts: the ledger, newest first, as many as fit.
    let ledger = Rect::new(inner.x, inner.y + band_h + axis, inner.width, ledger_h);
    let mut lefts = vec![
        Span::styled("in", t.secondary),
        Span::styled(format::count(agg.prompt_tps), t.secondary),
    ];
    if !bursts.is_empty() {
        let total = e.bursts.total_tokens(now).round();
        lefts.push(Span::styled(
            format!("Σ{}", format::count(Some(total))),
            t.dim,
        ));
    }
    let name_w = NAME_W as u16 + 1;
    frame.render_widget(
        Paragraph::new(name_column(lefts)),
        Rect {
            width: name_w,
            ..ledger
        },
    );
    let rows_x = inner.x + name_w;
    let rows_w = inner.width - name_w;
    if bursts.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("no prompts in the last {window_label}"),
                t.na,
            ))),
            Rect::new(rows_x, ledger.y, rows_w, 1),
        );
        return;
    }

    // The context window the bars are measured against: the largest among
    // the served models, else the `sglang:context_len` metric (the largest
    // across series, since PD roles report the same window).
    let ctx = e
        .served_models
        .iter()
        .filter_map(|m| m.max_model_len)
        .max()
        .map(|c| c as f64)
        .or_else(|| {
            e.curated.as_ref().and_then(|c| {
                c.series
                    .values()
                    .filter_map(|s| s.context_len)
                    .fold(None, |acc: Option<f64>, v| {
                        Some(acc.map_or(v, |a| a.max(v)))
                    })
            })
        });
    let biggest = bursts.iter().map(|b| b.tokens).fold(0.0, f64::max);

    // Attribution runs over the whole window, oldest first, so that an
    // older burst consumes its request before a newer one can claim it.
    let oldest_first: Vec<&Burst> = bursts.iter().rev().copied().collect();
    let entries: Vec<&RequestEntry> = e.requests.iter_newest_first().collect();
    let mut who = attribute(&oldest_first, &entries, history);
    who.reverse();

    for (i, (b, request)) in bursts
        .iter()
        .zip(who)
        .take(usize::from(ledger_h))
        .enumerate()
    {
        let row = Row {
            age: now.saturating_duration_since(b.at),
            tokens: b.tokens,
            share: ctx.map(|c| b.tokens / c),
            frac: ctx.map_or(b.tokens / biggest, |c| b.tokens / c),
            requests: b.requests,
            request,
        };
        frame.render_widget(
            Paragraph::new(ledger_line(t, usize::from(rows_w), &row)),
            Rect::new(rows_x, ledger.y + i as u16, rows_w, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn heights_keep_the_newest_sample_at_the_right_edge() {
        // Six dots, three samples: the left half has nothing yet.
        let h = heights(&[0.0, 5.0, 10.0], 6, 1, 10.0);
        assert_eq!(h, vec![None, None, None, Some(0), Some(2), Some(4)]);
        // More samples than dots: only the newest are shown.
        let h = heights(&[1.0, 2.0, 3.0, 4.0], 2, 1, 4.0);
        assert_eq!(h, vec![Some(3), Some(4)]);
        // Any positive rate is at least one dot; nothing exceeds the top.
        let h = heights(&[0.01, 99.0], 2, 2, 10.0);
        assert_eq!(h, vec![Some(1), Some(8)]);
    }

    fn glyphs(grid: &[Vec<(Option<char>, Shade)>]) -> Vec<String> {
        grid.iter()
            .map(|row| row.iter().map(|(g, _)| g.unwrap_or(' ')).collect())
            .collect()
    }

    #[test]
    fn braille_cells_fill_from_the_floor_and_mark_the_crest() {
        // One row: an empty dot, a measured zero, then heights 2 and 4.
        let grid = compose(&[None, Some(0), Some(2), Some(4)], 1, false);
        assert_eq!(glyphs(&grid), vec!["⢀⣼"]);
        assert_eq!(grid[0][0].1, Shade::Baseline);
        assert_eq!(grid[0][1].1, Shade::Crest);

        // Two rows: heights 6 and 8 of 8. The top cell holds both crests,
        // the bottom cell is pure fill.
        let grid = compose(&[Some(6), Some(8)], 2, false);
        assert_eq!(glyphs(&grid), vec!["⣼", "⣿"]);
        assert_eq!(grid[0][0].1, Shade::Crest);
        assert_eq!(grid[1][0].1, Shade::Fill);

        // A whole empty cell stays `None`, so the background shows through.
        let grid = compose(&[None, None, Some(4)], 1, false);
        assert_eq!(grid[0][0].0, None);
        assert_eq!(glyphs(&grid), vec![" ⡇"]);
    }

    #[test]
    fn ascii_cells_use_hashes_and_an_underscore_baseline() {
        let grid = compose(&[None, Some(0), Some(8), Some(8)], 2, true);
        assert_eq!(glyphs(&grid), vec![" #", "_#"]);
        assert_eq!(grid[0][1].1, Shade::Crest);
        assert_eq!(grid[1][1].1, Shade::Fill);
        assert_eq!(grid[1][0].1, Shade::Baseline);
    }

    #[test]
    fn the_band_scrolls_one_dot_per_sample() {
        // A 2-cell (4-dot) band: each new sample lands in the rightmost
        // dot and everything older shifts left by exactly one dot.
        let t = Theme::truecolor();
        let render = |values: &[f64]| -> String {
            let mut term = Terminal::new(TestBackend::new(2, 1)).unwrap();
            term.draw(|f| draw_area(f, &t, Rect::new(0, 0, 2, 1), values, 4.0))
                .unwrap();
            let buf = term.backend().buffer();
            (0..2).map(|x| buf[(x, 0)].symbol().to_string()).collect()
        };
        assert_eq!(render(&[4.0]), " ⢸");
        assert_eq!(render(&[4.0, 0.0]), " ⣇");
        assert_eq!(render(&[4.0, 0.0, 2.0]), "⢸⣠");
        assert_eq!(render(&[4.0, 0.0, 2.0, 1.0]), "⣇⣄");
        // A fifth sample pushes the oldest off the left edge.
        assert_eq!(render(&[4.0, 0.0, 2.0, 1.0, 3.0]), "⣠⣰");
    }

    #[test]
    fn labels_fit_their_columns() {
        for v in [
            1.0, 9.9, 64.9, 99.95, 100.0, 999.0, 9023.7, 13_000.0, 99_999.0, 1.2e6, 99.9e6,
        ] {
            assert!(
                axis_label(v).chars().count() <= SCALE_W,
                "{v}: {}",
                axis_label(v)
            );
            assert!(
                tokens_label(v).chars().count() <= 6,
                "{v}: {}",
                tokens_label(v)
            );
        }
        assert_eq!(axis_label(9023.7), "9.0k");
        assert_eq!(tokens_label(412.0), "412");
        assert_eq!(tokens_label(9800.0), "9.8k");
        assert_eq!(tokens_label(55_200.0), "55.2k");
        assert_eq!(tokens_label(999_960.0), "1.00M");
        assert_eq!(tokens_label(1.2e6), "1.20M");
    }

    #[test]
    fn gutter_rows_are_exactly_the_gutter_wide() {
        let t = Theme::truecolor();
        let lines = gutter(
            &t,
            4,
            vec![
                Span::styled("out", t.value),
                Span::styled("1234.5", t.value),
            ],
            "13.0k",
        );
        for l in &lines {
            assert_eq!(l.width(), usize::from(GUTTER), "{l:?}");
        }
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text[0], "out     13.0k┤");
        assert_eq!(text[2], "             │");
        assert_eq!(text[3], "            0┤");
    }

    fn entry(id: &str, seen_at: Instant, finished: bool) -> RequestEntry {
        RequestEntry {
            id: id.into(),
            seen_at,
            max_tokens: None,
            prompt_preview: None,
            status: if finished {
                RequestStatus::Finished { reason: None }
            } else {
                RequestStatus::Generating
            },
        }
    }

    fn burst(at: Instant, tokens: f64, requests: Option<u32>) -> Burst {
        Burst {
            at,
            tokens,
            requests,
        }
    }

    #[test]
    fn attribution_names_a_request_only_when_it_is_the_only_candidate() {
        let base = Instant::now();
        let s = Duration::from_secs;
        let window = s(300);
        // A alone, then B and C queued together, then D alone.
        let a = entry("a", base + s(10), true);
        let b = entry("b", base + s(30), false);
        let c = entry("c", base + s(31), false);
        let d = entry("d", base + s(80), false);
        let entries = [&d, &c, &b, &a]; // newest first, as the ring iterates
        let b1 = burst(base + s(12), 5000.0, Some(1));
        let b2 = burst(base + s(33), 9000.0, Some(1)); // b or c: ambiguous
        let b3 = burst(base + s(40), 4000.0, Some(1)); // the other one
        let b4 = burst(base + s(82), 7000.0, Some(1));
        let who = attribute(&[&b1, &b2, &b3, &b4], &entries, window);
        let ids: Vec<Option<&str>> = who.iter().map(|r| r.map(|r| r.id.as_str())).collect();
        assert_eq!(ids, vec![Some("a"), None, Some("c"), Some("d")]);

        // A two-prompt burst consumes both of its candidates and names none.
        let b23 = burst(base + s(33), 13_000.0, Some(2));
        let who = attribute(&[&b1, &b23, &b4], &entries, window);
        let ids: Vec<Option<&str>> = who.iter().map(|r| r.map(|r| r.id.as_str())).collect();
        assert_eq!(ids, vec![Some("a"), None, Some("d")]);

        // A request received later than the burst (plus slack) is not it.
        let early = burst(base + s(5), 100.0, Some(1));
        assert!(
            attribute(&[&early], &entries, window)
                .iter()
                .all(Option::is_none)
        );

        // A request received more than a window before the burst belongs
        // to a burst that already fell out of the ring.
        let late = burst(base + s(400), 100.0, Some(1));
        assert!(
            attribute(&[&late], &[&a], window)
                .iter()
                .all(Option::is_none)
        );
        // Unknown prompt counts never name anybody.
        let unknown = burst(base + s(12), 100.0, None);
        assert!(
            attribute(&[&unknown], &[&a], window)
                .iter()
                .all(Option::is_none)
        );
    }

    fn text_of(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn ledger_rows_fill_their_width_exactly_and_degrade_in_order() {
        let t = Theme::truecolor();
        let base = Instant::now();
        let req = entry("chatcmpl-8e66f5c2", base, false);
        let row = Row {
            age: Duration::from_secs(12),
            tokens: 55_200.0,
            share: Some(0.21),
            frac: 0.21,
            requests: Some(1),
            request: Some(&req),
        };
        for width in [14usize, 20, 24, 30, 40, 60, 96] {
            let line = ledger_line(&t, width, &row);
            assert_eq!(line.width(), width.max(ROW_HEAD_W), "{width}: {line:?}");
        }
        let wide = text_of(&ledger_line(&t, 60, &row));
        assert!(wide.starts_with("  -12s  55.2k █"), "{wide}");
        assert!(wide.contains(" 21% ctx  chatcmpl-8e66f5c2"), "{wide}");
        // 32 cells: the request is dropped, the share stays.
        let mid = text_of(&ledger_line(&t, 32, &row));
        assert!(mid.contains("% ctx") && !mid.contains("chatcmpl"), "{mid}");
        // 20 cells: only the bar is left.
        let tight = text_of(&ledger_line(&t, 20, &row));
        assert!(!tight.contains("ctx"), "{tight}");

        // Several prompts in one burst, no context window known.
        let shared = Row {
            age: Duration::from_secs(101),
            tokens: 9800.0,
            share: None,
            frac: 0.5,
            requests: Some(2),
            request: None,
        };
        let s = text_of(&ledger_line(&t, 60, &shared));
        assert!(s.starts_with("-1m41s   9.8k "), "{s}");
        assert!(s.ends_with("  2 reqs"), "{s}");
        assert!(!s.contains("ctx"), "{s}");
        // Tiny prompts never round to a 0% share.
        let tiny = Row {
            share: Some(0.001),
            ..shared
        };
        assert!(text_of(&ledger_line(&t, 60, &tiny)).contains(" <1% ctx"));
    }
}

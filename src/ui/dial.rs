//! TTFT as speedometers.
//!
//! Time to first token spans four orders of magnitude in practice: tens of
//! milliseconds for a short prompt on an idle server, tens of seconds behind
//! a queue. So the dial is logarithmic, giving every decade the same sweep.
//! The arc changes colour at the warn/crit thresholds, and ticks mark them
//! too, so the zones survive a monochrome terminal. A small mark outside the
//! arc is the window's peak: the one piece of history a lone needle loses.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Context, Line as Stroke, Points};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::ui::format;
use crate::ui::theme::Theme;

/// Sweep of the arc, in degrees: a speedometer's 240°, not a semicircle.
const SWEEP: f64 = 240.0;
/// Angle of the scale's low end (lower left), in degrees.
const START: f64 = 210.0;
/// Smallest radius, in braille dots, that still reads as a dial.
const MIN_RADIUS: f64 = 10.0;

/// A logarithmic scale with two thresholds, all in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scale {
    pub lo: f64,
    pub hi: f64,
    pub warn: f64,
    pub crit: f64,
}

impl Scale {
    /// TTFT: 10 ms to 60 s.
    pub fn ttft(warn: f64, crit: f64) -> Scale {
        Scale {
            lo: 0.01,
            hi: 60.0,
            warn,
            crit,
        }
    }

    /// Position on the scale, 0..=1, clamped at both ends.
    pub fn pos(&self, v: f64) -> f64 {
        if v.is_nan() || v <= self.lo {
            return 0.0;
        }
        ((v.ln() - self.lo.ln()) / (self.hi.ln() - self.lo.ln())).clamp(0.0, 1.0)
    }

    fn value_at(&self, p: f64) -> f64 {
        (self.lo.ln() + p * (self.hi.ln() - self.lo.ln())).exp()
    }

    pub fn style(&self, t: &Theme, v: f64) -> Style {
        if v >= self.crit {
            t.crit
        } else if v >= self.warn {
            t.warn
        } else {
            t.value
        }
    }
}

/// One dial's input. `value: None` means no request landed in the window:
/// the dial shows `--` and no needle, never a needle resting at zero.
#[derive(Debug, Clone, Copy)]
pub struct Reading<'a> {
    pub label: &'a str,
    pub value: Option<f64>,
    /// Highest value seen over the history window.
    pub peak: Option<f64>,
}

/// Dial geometry in braille dots (2 per cell across, 4 down; roughly
/// square), y growing upward from the bottom of the inner area.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Geometry {
    cx: f64,
    cy: f64,
    r: f64,
}

/// Geometry for an inner area of `w`×`h` cells, or `None` when a dial would
/// be too small to read. A 240° arc spans 1.5 r vertically (r above the
/// hub, r·sin 30° below), plus a text row of end labels underneath and two
/// dots outside the arc for the peak mark.
fn geometry(w: u16, h: u16) -> Option<Geometry> {
    let (dw, dh) = (f64::from(w) * 2.0, f64::from(h) * 4.0);
    let r = ((dw / 2.0) - 3.0).min((dh - 4.0 - 3.0) / 1.5).floor();
    if r < MIN_RADIUS {
        return None;
    }
    let used = 4.0 + 1.5 * r + 2.0;
    let base = ((dh - used) / 2.0).floor().max(0.0);
    Some(Geometry {
        cx: (dw / 2.0).floor(),
        cy: base + 4.0 + 0.5 * r,
        r,
    })
}

fn fg(style: Style) -> Color {
    style.fg.unwrap_or(Color::Reset)
}

fn point(g: &Geometry, p: f64, radius: f64) -> (f64, f64) {
    let a = (START - SWEEP * p).to_radians();
    (g.cx + radius * a.cos(), g.cy + radius * a.sin())
}

fn radial(g: &Geometry, p: f64, from: f64, to: f64, color: Color) -> Stroke {
    let (x1, y1) = point(g, p, from);
    let (x2, y2) = point(g, p, to);
    Stroke {
        x1,
        y1,
        x2,
        y2,
        color,
    }
}

fn paint(ctx: &mut Context, t: &Theme, g: &Geometry, scale: &Scale, reading: &Reading) {
    // The arc, two dots thick, coloured by zone; all dim without a reading.
    let steps = (SWEEP.to_radians() * g.r * 2.0).ceil() as usize;
    let mut zones: [Vec<(f64, f64)>; 3] = Default::default();
    for i in 0..=steps {
        let p = i as f64 / steps as f64;
        let v = scale.value_at(p);
        let zone = if v >= scale.crit {
            2
        } else if v >= scale.warn {
            1
        } else {
            0
        };
        for radius in [g.r, g.r - 1.0] {
            zones[zone].push(point(g, p, radius));
        }
    }
    let colors = if reading.value.is_some() {
        [fg(t.value), fg(t.warn), fg(t.crit)]
    } else {
        [fg(t.na); 3]
    };
    for (coords, color) in zones.iter().zip(colors) {
        ctx.draw(&Points { coords, color });
    }
    // Threshold ticks inside the arc: the zones, without colour.
    for th in [scale.warn, scale.crit] {
        ctx.draw(&radial(g, scale.pos(th), g.r - 2.0, g.r - 4.0, fg(t.dim)));
    }

    ctx.layer();
    if let (Some(peak), Some(_)) = (reading.peak, reading.value) {
        ctx.draw(&radial(
            g,
            scale.pos(peak),
            g.r + 1.0,
            g.r + 2.0,
            fg(t.secondary),
        ));
    }
    if let Some(v) = reading.value {
        let color = fg(scale.style(t, v));
        ctx.draw(&radial(g, scale.pos(v), 0.0, g.r - 3.0, color));
        ctx.draw(&Points {
            coords: &[
                (g.cx, g.cy),
                (g.cx + 1.0, g.cy),
                (g.cx, g.cy + 1.0),
                (g.cx + 1.0, g.cy + 1.0),
            ],
            color,
        });
    }
}

/// Scale end labels, as short as they can be: `10ms`, `60s`.
fn tick_label(v: f64) -> String {
    if v < 1.0 {
        format!("{:.0}ms", v * 1e3)
    } else {
        format!("{v:.0}s")
    }
}

/// The readout under the hub and the scale's end labels under the arc's two
/// ends, as `(column, row, line)` in cells of the inner area. Placed in exact
/// cells from the same dot geometry the canvas uses (x/2, y/4 from the top),
/// so a label can never land on the arc.
fn texts(
    t: &Theme,
    g: &Geometry,
    h: u16,
    scale: &Scale,
    reading: &Reading,
) -> Vec<(u16, u16, Line<'static>)> {
    let dh = f64::from(h) * 4.0;
    let col = |x: f64| (x / 2.0).floor().max(0.0) as u16;
    let row = |y: f64| ((dh - 1.0 - y) / 4.0).floor().max(0.0) as u16;

    let readout = format::seconds(reading.value);
    let readout_style = reading.value.map_or(t.na, |v| scale.style(t, v));
    let half = (readout.chars().count() / 2) as u16;
    let readout_at = (col(g.cx).saturating_sub(half), row(g.cy - 0.35 * g.r));

    let label_row = row(g.cy - 0.5 * g.r) + 1;
    let lo = tick_label(scale.lo);
    let hi = tick_label(scale.hi);
    let lo_col = col(point(g, 0.0, g.r).0).saturating_sub(1);
    let hi_col = (col(point(g, 1.0, g.r).0) + 2).saturating_sub(hi.chars().count() as u16);
    vec![
        (
            readout_at.0,
            readout_at.1,
            Line::styled(readout, readout_style),
        ),
        (lo_col, label_row, Line::styled(lo, t.dim)),
        (hi_col, label_row, Line::styled(hi, t.dim)),
    ]
}

/// Rows (borders included) at which two dials side by side in `width`
/// reach their full, width-bound size. Past this, height buys nothing.
pub fn pair_rows_wanted(width: u16) -> u16 {
    let inner_w = f64::from((width / 2).saturating_sub(2));
    let r = (inner_w - 3.0).max(MIN_RADIUS);
    ((1.5 * r + 7.0) / 4.0).ceil() as u16 + 2
}

/// One dial in a bordered box titled `TTFT p50`, with the window's peak on
/// the border. Too small for a dial, it draws the linear gauge instead.
pub fn draw(
    frame: &mut Frame,
    t: &Theme,
    area: Rect,
    reading: &Reading,
    scale: &Scale,
    window: &str,
) {
    let name = Span::styled(format!(" TTFT {} ", reading.label), t.heading);
    let name_w = name.width();
    let mut block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(name);
    if let (Some(peak), Some(_)) = (reading.peak, reading.value) {
        let peak_title = Line::from(vec![
            Span::styled(format!(" {window} peak "), t.dim),
            Span::styled(format::seconds(Some(peak)), t.secondary),
            Span::raw(" "),
        ]);
        // Only when it fits beside the name: overlapping titles clip both.
        if name_w + peak_title.width() + 2 <= usize::from(area.width) {
            block = block.title_top(peak_title.right_aligned());
        }
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    match geometry(inner.width, inner.height) {
        Some(g) => {
            let (dw, dh) = (f64::from(inner.width) * 2.0, f64::from(inner.height) * 4.0);
            let canvas = Canvas::default()
                .marker(Marker::Braille)
                .background_color(t.bg.bg.unwrap_or(Color::Reset))
                .x_bounds([0.0, dw - 1.0])
                .y_bounds([0.0, dh - 1.0])
                .paint(|ctx| paint(ctx, t, &g, scale, reading));
            frame.render_widget(canvas, inner);
            for (x, y, line) in texts(t, &g, inner.height, scale, reading) {
                if y >= inner.height || x >= inner.width {
                    continue;
                }
                let width = (line.width() as u16).min(inner.width - x);
                frame.render_widget(
                    Paragraph::new(line),
                    Rect {
                        x: inner.x + x,
                        y: inner.y + y,
                        width,
                        height: 1,
                    },
                );
            }
        }
        None => {
            let row = Rect {
                y: inner.y + (inner.height - 1) / 2,
                height: 1,
                ..inner
            };
            let unlabeled = Reading {
                label: "",
                ..*reading
            };
            frame.render_widget(
                Paragraph::new(gauge_line(t, &unlabeled, scale, inner.width)),
                row,
            );
        }
    }
}

/// The dial as one row: `p50 ━━━━━●┄┄┄│┄┄ 1.50s`. Cells before the needle
/// take their own zone's colour; the `│` is the window's peak.
pub fn gauge_line(t: &Theme, reading: &Reading, scale: &Scale, width: u16) -> Line<'static> {
    let w = usize::from(width);
    let label = if reading.label.is_empty() {
        String::new()
    } else {
        format!("{} ", reading.label)
    };
    let value_text = format::seconds(reading.value);
    let value_style = reading.value.map_or(t.na, |v| scale.style(t, v));
    let bar_w = w.saturating_sub(label.chars().count() + 1 + value_text.chars().count());

    let mut spans = vec![Span::styled(label, t.dim)];
    if bar_w >= 4 {
        let (done, needle, track, peak_mark) = if t.ascii() {
            ("=", "o", "-", "|")
        } else {
            ("━", "●", "┄", "│")
        };
        let last = (bar_w - 1) as f64;
        let at = |v: f64| (scale.pos(v) * last).round() as usize;
        let needle_at = reading.value.map(at);
        let peak_at = reading.peak.filter(|_| reading.value.is_some()).map(at);
        let track_style = if reading.value.is_some() { t.dim } else { t.na };

        let mut cells: Vec<(&str, Style)> = Vec::with_capacity(bar_w);
        for i in 0..bar_w {
            let cell = match needle_at {
                Some(k) if i < k => (done, scale.style(t, scale.value_at(i as f64 / last))),
                Some(k) if i == k => (needle, value_style),
                _ if peak_at == Some(i) => (peak_mark, t.secondary),
                _ => (track, track_style),
            };
            cells.push(cell);
        }
        // Merge runs of one style into one span.
        let mut run = String::new();
        let mut run_style = cells[0].1;
        for (glyph, style) in cells {
            if style != run_style {
                spans.push(Span::styled(std::mem::take(&mut run), run_style));
                run_style = style;
            }
            run.push_str(glyph);
        }
        spans.push(Span::styled(run, run_style));
        spans.push(Span::raw(" "));
    } else {
        spans.push(Span::raw(" ".repeat(bar_w + 1)));
    }
    spans.push(Span::styled(value_text, value_style));
    Line::from(spans)
}

/// Both dials. In a box tall and wide enough, two dials side by side or
/// stacked — whichever gives the bigger radius. Short of that, one or two
/// rows of linear gauges.
pub fn draw_pair(
    frame: &mut Frame,
    t: &Theme,
    area: Rect,
    readings: &[Reading; 2],
    scale: &Scale,
    window: &str,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    if area.height < 5 || area.width < 24 {
        frame.render_widget(Paragraph::new(gauge_lines(t, readings, scale, area)), area);
        return;
    }
    let side = Layout::horizontal([Constraint::Fill(1); 2]).split(area);
    let stack = Layout::vertical([Constraint::Fill(1); 2]).split(area);
    let radius = |r: Rect| {
        geometry(r.width.saturating_sub(2), r.height.saturating_sub(2)).map_or(0.0, |g| g.r)
    };
    let cells = if radius(stack[0]) > radius(side[0]) {
        stack
    } else {
        side
    };
    for (cell, reading) in cells.iter().zip(readings) {
        draw(frame, t, *cell, reading, scale, window);
    }
}

/// `TTFT p50 ━━●┄┄ 1.50s  p95 ━━━●┄ 2.40s` on one row when it fits, else
/// one row per percentile.
pub fn gauge_lines(
    t: &Theme,
    readings: &[Reading; 2],
    scale: &Scale,
    area: Rect,
) -> Vec<Line<'static>> {
    let prefix = "TTFT ";
    let w = area.width;
    if area.height >= 2 && w < 60 {
        return readings
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let lead = if i == 0 { prefix } else { "     " };
                let mut spans = vec![Span::styled(lead, t.heading)];
                spans.extend(gauge_line(t, r, scale, w.saturating_sub(5)).spans);
                Line::from(spans)
            })
            .collect();
    }
    let half = w.saturating_sub(7) / 2;
    let mut spans = vec![Span::styled(prefix, t.heading)];
    spans.extend(gauge_line(t, &readings[0], scale, half).spans);
    spans.push(Span::raw("  "));
    spans.extend(gauge_line(t, &readings[1], scale, w.saturating_sub(7) - half).spans);
    vec![Line::from(spans)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    const SCALE: Scale = Scale {
        lo: 0.01,
        hi: 60.0,
        warn: 2.0,
        crit: 10.0,
    };

    #[test]
    fn the_scale_is_logarithmic_and_clamped() {
        assert_eq!(SCALE.pos(0.01), 0.0);
        assert_eq!(SCALE.pos(0.001), 0.0);
        assert_eq!(SCALE.pos(60.0), 1.0);
        assert_eq!(SCALE.pos(600.0), 1.0);
        assert_eq!(SCALE.pos(f64::NAN), 0.0);
        // Every decade gets the same sweep.
        let d1 = SCALE.pos(1.0) - SCALE.pos(0.1);
        let d2 = SCALE.pos(10.0) - SCALE.pos(1.0);
        assert!((d1 - d2).abs() < 1e-9);
        assert!((SCALE.value_at(SCALE.pos(2.4)) - 2.4).abs() < 1e-9);
    }

    #[test]
    fn geometry_fits_inside_its_area_or_declines() {
        for w in 0u16..=80 {
            for h in 0u16..=30 {
                let Some(g) = geometry(w, h) else { continue };
                let (dw, dh) = (f64::from(w) * 2.0, f64::from(h) * 4.0);
                assert!(g.r >= MIN_RADIUS);
                // Arc, peak mark included, stays inside the dot grid.
                assert!(g.cx - g.r - 2.0 >= 0.0 && g.cx + g.r + 2.0 <= dw, "{w}x{h}");
                assert!(g.cy + g.r + 2.0 <= dh, "{w}x{h}: {g:?}");
                // Text rows — the end labels sit one row under the arc's low
                // ends — stay inside the area.
                let t = Theme::truecolor();
                let r = Reading {
                    label: "p50",
                    value: Some(1.0),
                    peak: None,
                };
                for (x, y, line) in texts(&t, &g, h, &SCALE, &r) {
                    assert!(y < h, "{w}x{h}: row {y}");
                    assert!(
                        usize::from(x) + line.width() <= usize::from(w) + 1,
                        "{w}x{h}: col {x}"
                    );
                }
            }
        }
        assert!(geometry(4, 2).is_none());
    }

    #[test]
    fn pair_rows_wanted_is_enough_for_a_full_size_dial() {
        for width in [40u16, 80, 110, 160] {
            let rows = pair_rows_wanted(width);
            let g = geometry(width / 2 - 2, rows - 2).expect("a dial");
            let more = geometry(width / 2 - 2, rows + 10).expect("a dial");
            assert_eq!(g.r, more.r, "{width}: more height must not grow the dial");
        }
    }

    #[test]
    fn gauge_line_fills_its_width_and_marks_the_peak() {
        let t = Theme::truecolor();
        let r = Reading {
            label: "p50",
            value: Some(1.5),
            peak: Some(9.0),
        };
        for w in [12u16, 30, 57] {
            assert_eq!(gauge_line(&t, &r, &SCALE, w).width(), usize::from(w), "{w}");
        }
        let s = text(&gauge_line(&t, &r, &SCALE, 40));
        assert!(s.starts_with("p50 ━"), "{s}");
        assert!(s.contains('●') && s.contains('│'), "{s}");
        assert!(s.ends_with(" 1.50s"), "{s}");
    }

    #[test]
    fn no_reading_means_no_needle_and_dashes() {
        let t = Theme::truecolor();
        let r = Reading {
            label: "p95",
            value: None,
            peak: Some(3.0),
        };
        let s = text(&gauge_line(&t, &r, &SCALE, 30));
        assert!(
            !s.contains('●') && !s.contains('━') && !s.contains('│'),
            "{s}"
        );
        assert!(s.ends_with("--"), "{s}");
    }

    #[test]
    fn dial_renders_readout_and_end_labels() {
        let t = Theme::truecolor();
        let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let readings = [
            Reading {
                label: "p50",
                value: Some(1.5),
                peak: Some(2.0),
            },
            Reading {
                label: "p95",
                value: None,
                peak: None,
            },
        ];
        term.draw(|f| draw_pair(f, &t, f.area(), &readings, &SCALE, "5m"))
            .unwrap();
        let buf = term.backend().buffer();
        let out: String = (0..12)
            .map(|y| (0..40).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect();
        for expected in ["TTFT p50", "TTFT p95", "1.50s", "10ms", "60s", "--"] {
            assert!(out.contains(expected), "missing {expected}:\n{out}");
        }
    }
}

//! Concurrency as seats.
//!
//! The scheduler has a fixed number of slots (`--max-num-seqs`) and every
//! running request holds exactly one: a small integer, not a continuous
//! level. So each slot is one box, filled while occupied, and "3 of 12" reads
//! at a glance in a way a line chart of a small integer never did.
//!
//! SGLang does not export its slot count, so it comes from `@N` /
//! `max_running`. Without it only the occupied seats are drawn: empty seats
//! we cannot count are never invented.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::ui::format;
use crate::ui::theme::Theme;

/// Occupancy at which seats turn amber / red.
const WARN_AT: f64 = 0.75;
const CRIT_AT: f64 = 0.95;
/// The tile never grows past this many rows of boxes; beyond that, each box
/// stands for several seats.
pub const MAX_ROWS: u16 = 4;
/// Width of the engine label (`e1  `) on data-parallel servers.
const LABEL_W: u16 = 4;

/// One engine's slots. A single-engine server has exactly one group.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SeatGroup {
    /// Engine label on data-parallel servers; `None` for a single engine.
    pub label: Option<String>,
    pub running: Option<u32>,
    /// This engine's slots (`--max-num-seqs`), when declared.
    pub cap: Option<u32>,
}

impl SeatGroup {
    /// How many boxes to lay out: every slot when the cap is known, else
    /// only the occupied ones.
    fn boxes(&self) -> u32 {
        self.cap.or(self.running).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Seats {
    pub groups: Vec<SeatGroup>,
    pub waiting: Option<u32>,
}

impl Seats {
    pub fn running(&self) -> Option<u32> {
        self.groups
            .iter()
            .filter_map(|g| g.running)
            .fold(None, |acc, r| Some(acc.unwrap_or(0) + r))
    }

    /// Total slots — only when every group declares its cap.
    pub fn cap(&self) -> Option<u32> {
        if self.groups.is_empty() {
            return None;
        }
        self.groups.iter().map(|g| g.cap).sum()
    }

    fn style(&self, t: &Theme, running: u32, cap: Option<u32>) -> Style {
        match cap {
            Some(cap) if cap > 0 => {
                t.by_level(f64::from(running) / f64::from(cap), WARN_AT, CRIT_AT)
            }
            _ => t.value,
        }
    }

    /// Rows of boxes this tile needs at an inner `width`.
    pub fn rows_needed(&self, width: u16) -> u16 {
        let (w, per_group) = self.group_geometry(width);
        let rows: u16 = self
            .groups
            .iter()
            .map(|g| pack(g.boxes(), w, per_group).rows.max(1))
            .sum();
        rows.clamp(1, MAX_ROWS.max(self.groups.len() as u16))
    }

    /// Box width and per-group row budget: data-parallel groups share the
    /// tile, one or more rows each, behind an engine label.
    fn group_geometry(&self, width: u16) -> (u16, u16) {
        if self.groups.len() > 1 {
            let per = (MAX_ROWS / self.groups.len() as u16).max(1);
            (width.saturating_sub(LABEL_W), per)
        } else {
            (width, MAX_ROWS)
        }
    }
}

/// How `n` boxes pack into `width` cells over at most `max_rows` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packing {
    /// Cells per box: 2 (`■ `) when roomy, 1 when tight.
    pub stride: u16,
    /// Seats per box: 1 unless even one cell per seat would not fit.
    pub per_box: u32,
    pub boxes: u32,
    pub per_row: u32,
    pub rows: u16,
}

pub fn pack(n: u32, width: u16, max_rows: u16) -> Packing {
    if n == 0 || width == 0 || max_rows == 0 {
        return Packing {
            stride: 1,
            per_box: 1,
            boxes: 0,
            per_row: 0,
            rows: 0,
        };
    }
    let rows_cap = u32::from(max_rows);
    let (stride, per_box) = if n <= u32::from(width / 2) * rows_cap {
        (2, 1)
    } else if n <= u32::from(width) * rows_cap {
        (1, 1)
    } else {
        (1, n.div_ceil(u32::from(width) * rows_cap))
    };
    let boxes = n.div_ceil(per_box);
    let per_row = (u32::from(width) / u32::from(stride)).min(boxes);
    Packing {
        stride,
        per_box,
        boxes,
        per_row,
        rows: boxes.div_ceil(per_row) as u16,
    }
}

struct Glyphs {
    run: &'static str,
    free: &'static str,
    wait: &'static str,
}

fn glyphs(t: &Theme) -> Glyphs {
    if t.ascii() {
        Glyphs {
            run: "#",
            free: ".",
            wait: "+",
        }
    } else {
        Glyphs {
            run: "■",
            free: "□",
            wait: "▪",
        }
    }
}

/// The box rows for every group, plus the waiting queue after the last box.
fn box_lines(t: &Theme, seats: &Seats, width: u16) -> Vec<Line<'static>> {
    let g = glyphs(t);
    let (w, per_group) = seats.group_geometry(width);
    let multi = seats.groups.len() > 1;
    let mut lines: Vec<Line<'static>> = Vec::new();

    for group in &seats.groups {
        let label = |first: bool| -> Option<Span<'static>> {
            multi.then(|| {
                let text = match (&group.label, first) {
                    (Some(l), true) => format!(
                        "{:<width$}",
                        format::truncate(l, 3),
                        width = usize::from(LABEL_W)
                    ),
                    _ => " ".repeat(usize::from(LABEL_W)),
                };
                Span::styled(text, t.dim)
            })
        };
        let Some(running) = group.running else {
            let mut spans: Vec<Span<'static>> = label(true).into_iter().collect();
            spans.push(Span::styled(format::NA, t.na));
            lines.push(Line::from(spans));
            continue;
        };
        let p = pack(group.boxes(), w, per_group);
        let filled = running.div_ceil(p.per_box).min(p.boxes);
        let run_style = seats.style(t, running, group.cap);
        let gap = if p.stride == 2 { " " } else { "" };

        for row in 0..u32::from(p.rows) {
            let mut spans: Vec<Span<'static>> = label(row == 0).into_iter().collect();
            let (start, end) = (row * p.per_row, ((row + 1) * p.per_row).min(p.boxes));
            let run_n = filled.clamp(start, end) - start;
            let free_n = end - start - run_n;
            if run_n > 0 {
                spans.push(Span::styled(
                    format!("{}{gap}", g.run).repeat(run_n as usize),
                    run_style,
                ));
            }
            if free_n > 0 {
                spans.push(Span::styled(
                    format!("{}{gap}", g.free).repeat(free_n as usize),
                    t.dim,
                ));
            }
            lines.push(Line::from(spans));
        }
        // A declared cap below what the server is actually running: the
        // declaration is wrong, and saying so beats clipping silently.
        if let Some(cap) = group.cap
            && running > cap
            && let Some(last) = lines.last_mut()
        {
            last.spans
                .push(Span::styled(format!(" +{}", running - cap), t.crit));
        }
    }

    // The queue rides after the last box, as far as the row has room.
    if let Some(waiting) = seats.waiting.filter(|w| *w > 0)
        && let Some(last) = lines.last_mut()
    {
        let room = usize::from(width).saturating_sub(last.width() + 1);
        let more = format!("+{waiting}");
        let shown = if (waiting as usize) <= room {
            waiting as usize
        } else {
            room.saturating_sub(more.len())
        };
        if shown > 0 || room >= more.len() {
            last.spans.push(Span::raw(" "));
            last.spans.push(Span::styled(g.wait.repeat(shown), t.warn));
            if shown < waiting as usize {
                last.spans.push(Span::styled(more, t.warn));
            }
        }
    }
    lines
}

/// `SEATS 3 / 12 running · 0 waiting`, plus what a box means when it
/// stands for more than one seat.
fn title(t: &Theme, seats: &Seats, inner_w: u16) -> Line<'static> {
    let mut spans = vec![Span::styled(" SEATS ", t.heading)];
    let Some(running) = seats.running() else {
        spans.push(Span::styled(format!("{} ", format::NA), t.na));
        return Line::from(spans);
    };
    let cap = seats.cap();
    let run_text = match cap {
        Some(cap) => format!("{running} / {cap}"),
        None => running.to_string(),
    };
    spans.push(Span::styled(run_text, seats.style(t, running, cap)));
    spans.push(Span::styled(" running", t.dim));
    if let Some(w) = seats.waiting {
        spans.push(Span::styled(" · ", t.dim));
        spans.push(Span::styled(
            format!("{w} waiting"),
            if w > 0 { t.warn } else { t.dim },
        ));
    }
    if cap.is_none() {
        spans.push(Span::styled(" · cap ? (add @N)", t.dim));
    } else {
        let (w, per_group) = seats.group_geometry(inner_w);
        let per_box = seats
            .groups
            .iter()
            .map(|g| pack(g.boxes(), w, per_group).per_box)
            .max()
            .unwrap_or(1);
        if per_box > 1 {
            spans.push(Span::styled(
                format!(" · 1{}={per_box} seats", glyphs(t).run),
                t.dim,
            ));
        }
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

/// One row: `SEATS ■■■□□□□□□□□□ 3/12 · 2 wait`.
pub fn line(t: &Theme, seats: &Seats, width: u16) -> Line<'static> {
    let g = glyphs(t);
    let label = Span::styled("SEATS ", t.heading);
    let Some(running) = seats.running() else {
        return Line::from(vec![label, Span::styled(format::NA, t.na)]);
    };
    let cap = seats.cap();
    let mut figures = match cap {
        Some(cap) => format!(" {running}/{cap}"),
        None => format!(" {running}"),
    };
    if let Some(w) = seats.waiting.filter(|w| *w > 0) {
        figures.push_str(&format!(" · {w} wait"));
    }
    let style = seats.style(t, running, cap);
    let room = usize::from(width).saturating_sub(label.width() + figures.chars().count());
    let n = cap.unwrap_or(running);
    let mut spans = vec![label];
    if room >= 4 && n > 0 {
        let p = pack(n, room as u16, 1);
        let filled = running.div_ceil(p.per_box).min(p.boxes);
        let gap = if p.stride == 2 { " " } else { "" };
        spans.push(Span::styled(
            format!("{}{gap}", g.run).repeat(filled as usize),
            style,
        ));
        spans.push(Span::styled(
            format!("{}{gap}", g.free).repeat((p.boxes - filled) as usize),
            t.dim,
        ));
    }
    spans.push(Span::styled(figures, style));
    Line::from(spans)
}

/// The bordered tile: counts on the border, boxes inside. Falls back to
/// [`line`] below three rows.
pub fn draw(frame: &mut Frame, t: &Theme, area: Rect, seats: &Seats) {
    if area.height < 3 {
        frame.render_widget(Paragraph::new(line(t, seats, area.width)), area);
        return;
    }
    let inner_w = area.width.saturating_sub(2);
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(title(t, seats, inner_w));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines = box_lines(t, seats, inner.width);
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn one(running: Option<u32>, cap: Option<u32>, waiting: Option<u32>) -> Seats {
        Seats {
            groups: vec![SeatGroup {
                label: None,
                running,
                cap,
            }],
            waiting,
        }
    }

    #[test]
    fn packing_prefers_spacing_then_density_then_scaling() {
        // Twelve seats, roomy: two cells each, one row.
        assert_eq!(
            pack(12, 100, 4),
            Packing {
                stride: 2,
                per_box: 1,
                boxes: 12,
                per_row: 12,
                rows: 1
            }
        );
        // 256 seats in 100 cells: stride 2 would need 6 rows, so one cell
        // each over three rows.
        let p = pack(256, 100, 4);
        assert_eq!((p.stride, p.per_box, p.rows), (1, 1, 3));
        // 1024 seats cannot fit one per cell in 4×100: three per box.
        let p = pack(1024, 100, 4);
        assert_eq!((p.stride, p.per_box), (1, 3));
        assert!(p.rows <= 4);
        assert_eq!(pack(0, 100, 4).boxes, 0);
        assert_eq!(pack(5, 0, 4).boxes, 0);
        assert_eq!(pack(5, 1, 4).stride, 1);
    }

    #[test]
    fn packing_never_exceeds_its_budget() {
        for n in [1u32, 2, 7, 12, 64, 255, 256, 257, 1024, 4096] {
            for width in 1u16..=120 {
                for rows in 1u16..=4 {
                    let p = pack(n, width, rows);
                    assert!(p.rows <= rows, "{n} {width} {rows}: {p:?}");
                    assert!(p.per_row as u16 * p.stride <= width, "{n} {width}: {p:?}");
                    assert!(p.boxes * p.per_box >= n, "{n} {width}: {p:?}");
                }
            }
        }
    }

    #[test]
    fn a_known_cap_draws_every_seat() {
        let t = Theme::truecolor();
        let lines = box_lines(&t, &one(Some(3), Some(12), Some(0)), 60);
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]).trim_end(), "■ ■ ■ □ □ □ □ □ □ □ □ □");
    }

    #[test]
    fn without_a_cap_only_occupied_seats_are_drawn() {
        let t = Theme::truecolor();
        let s = one(Some(3), None, Some(2));
        let lines = box_lines(&t, &s, 60);
        assert_eq!(text(&lines[0]), "■ ■ ■  ▪▪");
        assert!(text(&title(&t, &s, 60)).contains("cap ? (add @N)"));
    }

    #[test]
    fn an_unscraped_endpoint_shows_dashes_not_empty_seats() {
        let t = Theme::truecolor();
        let s = one(None, Some(12), None);
        assert_eq!(text(&box_lines(&t, &s, 60)[0]), "--");
        assert!(text(&title(&t, &s, 60)).contains("--"));
        assert!(text(&line(&t, &s, 60)).ends_with("--"));
    }

    #[test]
    fn a_cap_below_reality_is_flagged_not_clipped() {
        let t = Theme::truecolor();
        let lines = box_lines(&t, &one(Some(14), Some(12), None), 60);
        assert!(text(&lines[0]).ends_with(" +2"), "{}", text(&lines[0]));
    }

    #[test]
    fn a_long_queue_is_capped_with_a_count() {
        let t = Theme::truecolor();
        let lines = box_lines(&t, &one(Some(2), Some(4), Some(500)), 30);
        let s = text(&lines[0]);
        assert!(s.ends_with("+500"), "{s}");
        assert!(lines[0].width() <= 30, "{s}");
    }

    #[test]
    fn data_parallel_engines_get_a_labelled_row_each() {
        let t = Theme::truecolor();
        let s = Seats {
            groups: vec![
                SeatGroup {
                    label: Some("e0".into()),
                    running: Some(2),
                    cap: Some(4),
                },
                SeatGroup {
                    label: Some("e1".into()),
                    running: Some(1),
                    cap: Some(4),
                },
            ],
            waiting: None,
        };
        let lines = box_lines(&t, &s, 40);
        assert_eq!(lines.len(), 2);
        assert_eq!(text(&lines[0]).trim_end(), "e0  ■ ■ □ □");
        assert_eq!(text(&lines[1]).trim_end(), "e1  ■ □ □ □");
        assert_eq!(s.running(), Some(3));
        assert_eq!(s.cap(), Some(8));
        assert_eq!(s.rows_needed(40), 2);
    }

    #[test]
    fn compact_line_fits_its_width() {
        let t = Theme::truecolor();
        let s = one(Some(3), Some(12), Some(2));
        for w in [20u16, 31, 60, 100] {
            assert!(line(&t, &s, w).width() <= usize::from(w), "{w}");
        }
        assert_eq!(
            text(&line(&t, &s, 60)),
            "SEATS ■ ■ ■ □ □ □ □ □ □ □ □ □  3/12 · 2 wait"
        );
    }
}

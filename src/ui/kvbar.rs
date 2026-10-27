//! KV cache as a loading bar.
//!
//! KV usage is a level, not a stream of events: one bar says what the old
//! history chart did, in a single row. The percentage and absolute tokens sit
//! beside it. SGLang does not report KV bytes (`kv_cache_memory_bytes` is the
//! literal string "None"), so there is no GB figure to show.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::ui::format;
use crate::ui::theme::Theme;

const WARN_AT: f64 = 0.75;
const CRIT_AT: f64 = 0.9;
/// Below this many cells a bar stops being worth its space.
const MIN_BAR: usize = 6;
/// A taller tile draws a thicker bar, up to this many rows.
const MAX_BAR_ROWS: u16 = 2;

/// What the bar shows. Every field `None`/`false` means "not scraped".
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Kv {
    pub frac: Option<f64>,
    /// `(used, capacity)` tokens — only when every series reports capacity,
    /// so the total is never a partial sum shown as a whole.
    pub tokens: Option<(f64, f64)>,
    /// `frac` is an unweighted mean across series (capacities unknown).
    pub unweighted: bool,
}

impl Kv {
    fn figures(&self, t: &Theme, with_tokens: bool) -> Vec<Span<'static>> {
        let Some(f) = self.frac else {
            return vec![Span::styled(format::NA, t.na)];
        };
        let mut spans = vec![Span::styled(
            format::percent(Some(f)),
            t.by_level(f, WARN_AT, CRIT_AT),
        )];
        match self.tokens {
            Some((used, cap)) if with_tokens => spans.push(Span::styled(
                format!(
                    "  {} / {} tok",
                    // Used tokens are derived (fraction × capacity), so round:
                    // a token count has no decimals.
                    format::count(Some(used.round())),
                    format::count(Some(cap))
                ),
                t.dim,
            )),
            None if self.unweighted && with_tokens => {
                spans.push(Span::styled("  unweighted", t.dim));
            }
            _ => {}
        }
        spans
    }

    fn bar(&self, t: &Theme, width: usize) -> Vec<Span<'static>> {
        match self.frac {
            // Unknown: an empty, dimmed track — never a bar at 0%.
            None => vec![Span::styled(
                format::bar_track(0.0, width, t.ascii()).1,
                t.na,
            )],
            Some(f) => {
                let (filled, track) = format::bar_track(f, width, t.ascii());
                vec![
                    Span::styled(filled, t.by_level(f, WARN_AT, CRIT_AT)),
                    Span::styled(track, t.dim),
                ]
            }
        }
    }
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// One row: `KV ███▍░░░░░░ 8.1%  47.5k / 586.5k tok`. The bar yields first,
/// then the token counts; the percentage always stays.
pub fn line(t: &Theme, kv: &Kv, width: u16) -> Line<'static> {
    let w = usize::from(width);
    let label = Span::styled("KV ", t.heading);
    let mut figures = kv.figures(t, true);
    if label.width() + width_of(&figures) > w {
        figures = kv.figures(t, false);
    }
    let fixed = label.width() + 1 + width_of(&figures);
    let mut spans = vec![label];
    if w >= fixed + MIN_BAR {
        spans.extend(kv.bar(t, w - fixed));
        spans.push(Span::raw(" "));
    }
    spans.extend(figures);
    Line::from(spans)
}

/// The bordered tile: name and figures on the border, the bar across the
/// whole inner width. Falls back to [`line`] below three rows.
pub fn draw(frame: &mut Frame, t: &Theme, area: Rect, kv: &Kv) {
    if area.height < 3 {
        frame.render_widget(Paragraph::new(line(t, kv, area.width)), area);
        return;
    }
    let name = Span::styled(" KV CACHE ", t.heading);
    let mut figures = kv.figures(t, true);
    if name.width() + width_of(&figures) + 4 > usize::from(area.width) {
        figures = kv.figures(t, false);
    }
    figures.insert(0, Span::raw(" "));
    figures.push(Span::raw(" "));
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(name)
        .title_top(Line::from(figures).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = inner.height.min(MAX_BAR_ROWS);
    let bar = Line::from(kv.bar(t, usize::from(inner.width)));
    let lines = vec![bar; usize::from(rows)];
    let top = inner.y + (inner.height - rows) / 2;
    frame.render_widget(
        Paragraph::new(lines),
        Rect {
            y: top,
            height: rows,
            ..inner
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn line_fills_the_width_and_keeps_the_percentage() {
        let t = Theme::truecolor();
        let kv = Kv {
            frac: Some(0.081),
            tokens: Some((47_500.0, 586_500.0)),
            unweighted: false,
        };
        let l = line(&t, &kv, 80);
        assert_eq!(l.width(), 80);
        let s = text(&l);
        assert!(s.starts_with("KV █"), "{s}");
        assert!(s.ends_with("8.1%  47.5k / 586.5k tok"), "{s}");

        // Too narrow for tokens or a bar: the percentage survives.
        let s = text(&line(&t, &kv, 12));
        assert_eq!(s, "KV 8.1%");
    }

    #[test]
    fn unknown_kv_is_dashes_on_an_empty_track_never_zero_percent() {
        let t = Theme::truecolor();
        let s = text(&line(&t, &Kv::default(), 40));
        assert!(s.ends_with("--"), "{s}");
        assert!(!s.contains('%'), "{s}");
        assert!(!s.contains('█'), "{s}");
    }
}

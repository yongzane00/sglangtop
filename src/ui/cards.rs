//! Stat cards: the top band of both views.
//!
//! A card is a bordered box with an eyebrow label (drawn as the block title,
//! so it costs no interior row), a big value, and one supporting line that is
//! either text or an accent (utilization bar / sparkline).
//!
//! Takes `&Theme` rather than `&App`, so the fleet view, the endpoint view
//! and tests can all build cards without an application.
//!
//! The constructors are the single place where `None` becomes `--`: an
//! unavailable value never gets a unit, a bar, or a sparkline, because a bar
//! drawn for an unknown fraction is a lie.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::ui::format;
use crate::ui::theme::Theme;

/// Total height of one card, borders included.
pub const CARD_H: u16 = 4;
/// Below this width a card cannot hold a label and a value; callers fall
/// back to a compact text line instead.
pub const MIN_CARD_W: u16 = 16;

/// The supporting line under a card's value.
pub enum Accent {
    None,
    /// Utilization bar. `frac: None` renders nothing at all.
    Bar {
        frac: Option<f64>,
        warn_at: f64,
        crit_at: f64,
    },
    /// Recent values, oldest first; drawn right-aligned so the newest value
    /// sits at the edge.
    Spark(Vec<f64>),
}

pub struct Card {
    pub label: &'static str,
    /// Pre-formatted; `--` when unavailable.
    pub value: String,
    /// Dropped first when the card is too narrow.
    pub unit: Option<&'static str>,
    pub value_style: Style,
    pub sub: Option<String>,
    pub sub_style: Style,
    pub accent: Accent,
}

impl Card {
    fn unavailable(t: &Theme, label: &'static str) -> Card {
        Card {
            label,
            value: format::NA.into(),
            unit: None,
            value_style: t.na,
            sub: None,
            sub_style: t.dim,
            accent: Accent::None,
        }
    }

    fn present(t: &Theme, label: &'static str, value: String, unit: Option<&'static str>) -> Card {
        Card {
            label,
            value,
            unit,
            value_style: t.value,
            sub: None,
            sub_style: t.dim,
            accent: Accent::None,
        }
    }

    /// A count, e.g. running requests or tokens/s.
    pub fn count(
        t: &Theme,
        label: &'static str,
        v: Option<f64>,
        unit: Option<&'static str>,
    ) -> Card {
        match v {
            None => Card::unavailable(t, label),
            Some(v) => Card::present(t, label, format::count(Some(v)), unit),
        }
    }

    /// A duration, e.g. a latency percentile.
    pub fn seconds(t: &Theme, label: &'static str, v: Option<f64>) -> Card {
        match v {
            None => Card::unavailable(t, label),
            Some(v) => Card::present(t, label, format::seconds(Some(v)), None),
        }
    }

    /// A 0..=1 fraction rendered as a percentage, coloured by level, with a
    /// matching bar underneath.
    pub fn percent(
        t: &Theme,
        label: &'static str,
        frac: Option<f64>,
        warn_at: f64,
        crit_at: f64,
    ) -> Card {
        match frac {
            None => Card::unavailable(t, label),
            Some(f) => Card {
                value_style: t.by_level(f, warn_at, crit_at),
                accent: Accent::Bar {
                    frac: Some(f),
                    warn_at,
                    crit_at,
                },
                ..Card::present(t, label, format::percent(Some(f)), None)
            },
        }
    }

    /// Free text (a model name, an `up/total` tally).
    pub fn text(t: &Theme, label: &'static str, v: Option<String>) -> Card {
        match v {
            None => Card::unavailable(t, label),
            Some(v) => Card::present(t, label, v, None),
        }
    }

    /// Attach a supporting line. Ignored on an unavailable card: `--` with an
    /// explanatory sub-line would imply we know more than we do.
    pub fn sub(mut self, sub: Option<String>, style: Style) -> Card {
        if self.value != format::NA
            && let Some(sub) = sub
        {
            self.sub = Some(sub);
            self.sub_style = style;
        }
        self
    }

    /// Attach a utilization bar to a non-percent card (e.g. `3/8` running).
    /// Ignored when the fraction is unknown.
    pub fn bar(mut self, frac: Option<f64>, warn_at: f64, crit_at: f64) -> Card {
        if self.value != format::NA
            && let Some(f) = frac
        {
            self.accent = Accent::Bar {
                frac: Some(f),
                warn_at,
                crit_at,
            };
        }
        self
    }

    /// Attach a sparkline (ignored when there is nothing to plot).
    pub fn spark(mut self, values: Vec<f64>) -> Card {
        if self.value != format::NA && !values.is_empty() {
            self.accent = Accent::Spark(values);
        }
        self
    }

    /// Override the value style (e.g. `warn` when a queue is non-empty).
    pub fn style(mut self, style: Style) -> Card {
        if self.value != format::NA {
            self.value_style = style;
        }
        self
    }
}

/// How many cards fit across `width`.
pub fn columns(width: u16) -> usize {
    if width >= 150 {
        6
    } else if width >= 100 {
        3
    } else if width >= MIN_CARD_W * 2 + 4 {
        2
    } else {
        0
    }
}

/// Grid shape for `n` cards at `width`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CardGrid {
    pub cols: usize,
    pub rows: usize,
    pub height: u16,
}

pub fn card_grid(width: u16, n: usize) -> CardGrid {
    let cols = columns(width);
    if cols == 0 || n == 0 {
        return CardGrid {
            cols: 0,
            rows: 0,
            height: 0,
        };
    }
    let rows = n.div_ceil(cols);
    CardGrid {
        cols,
        rows,
        height: CARD_H * rows as u16,
    }
}

/// Lay out and draw a band of cards, in the caller's priority order. Draws
/// only as many as fit in `area`; never overflows it.
pub fn draw_card_row(frame: &mut Frame, t: &Theme, area: Rect, cards: &[Card]) {
    let cols = columns(area.width);
    if cols == 0 || cards.is_empty() || area.height < CARD_H {
        return;
    }
    let rows_that_fit = (area.height / CARD_H) as usize;
    let mut drawn = 0usize;
    for row_idx in 0..rows_that_fit {
        if drawn >= cards.len() {
            break;
        }
        let row = Rect {
            x: area.x,
            y: area.y + row_idx as u16 * CARD_H,
            width: area.width,
            height: CARD_H,
        };
        // Fill(1) spreads the remainder instead of leaving a dead column.
        let cells = Layout::horizontal(vec![Constraint::Fill(1); cols]).split(row);
        for cell in cells.iter() {
            let Some(card) = cards.get(drawn) else { break };
            draw_card(frame, t, *cell, card);
            drawn += 1;
        }
    }
}

/// Draw a single card. Never wraps, never overflows, never panics.
pub fn draw_card(frame: &mut Frame, t: &Theme, area: Rect, card: &Card) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Span::styled(format!(" {} ", card.label), t.heading));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let w = inner.width as usize;

    // Value line: drop the unit before truncating the number itself.
    let mut value_spans = vec![Span::styled(card.value.clone(), card.value_style)];
    if let Some(unit) = card.unit
        && card.value.chars().count() + 1 + unit.chars().count() <= w
    {
        value_spans.push(Span::styled(format!(" {unit}"), t.dim));
    }
    if card.value.chars().count() > w {
        value_spans = vec![Span::styled(
            format::truncate(&card.value, w),
            card.value_style,
        )];
    }

    let mut lines = vec![Line::from(value_spans)];
    if inner.height >= 2 {
        lines.push(accent_line(t, card, w));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// The card's second line: the accent when there is one, else the sub-text.
fn accent_line<'a>(t: &Theme, card: &'a Card, w: usize) -> Line<'a> {
    match &card.accent {
        Accent::Bar {
            frac: Some(f),
            warn_at,
            crit_at,
        } => {
            // Bar plus the sub-text when both fit; the bar yields width first.
            let sub = card.sub.as_deref().unwrap_or("");
            let sub_cells = if sub.is_empty() {
                0
            } else {
                sub.chars().count() + 1
            };
            let bar_w = w.saturating_sub(sub_cells).clamp(0, 16);
            if bar_w < 4 {
                return sub_line(t, card, w);
            }
            let mut spans = vec![Span::styled(
                format::bar(*f, bar_w, t.ascii()),
                t.by_level(*f, *warn_at, *crit_at),
            )];
            if !sub.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", format::truncate(sub, w.saturating_sub(bar_w + 1))),
                    card.sub_style,
                ));
            }
            Line::from(spans)
        }
        Accent::Spark(values) if w >= 4 => {
            let sub = card.sub.as_deref().unwrap_or("");
            let sub_cells = if sub.is_empty() {
                0
            } else {
                sub.chars().count() + 1
            };
            let spark_w = w.saturating_sub(sub_cells).clamp(0, 16);
            if spark_w < 4 {
                return sub_line(t, card, w);
            }
            let mut spans = vec![Span::styled(
                format::spark(values, spark_w, t.ascii()),
                t.secondary,
            )];
            if !sub.is_empty() {
                spans.push(Span::styled(
                    format!(" {}", format::truncate(sub, w.saturating_sub(spark_w + 1))),
                    card.sub_style,
                ));
            }
            Line::from(spans)
        }
        _ => sub_line(t, card, w),
    }
}

fn sub_line<'a>(t: &Theme, card: &'a Card, w: usize) -> Line<'a> {
    match &card.sub {
        Some(sub) => Line::from(Span::styled(
            format::truncate(sub, w),
            if card.value == format::NA {
                t.na
            } else {
                card.sub_style
            },
        )),
        None => Line::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(area_w: u16, area_h: u16, card: &Card) -> String {
        let t = Theme::mono();
        let mut term = Terminal::new(TestBackend::new(area_w, area_h)).unwrap();
        term.draw(|f| {
            draw_card(
                f,
                &t,
                Rect {
                    x: 0,
                    y: 0,
                    width: area_w,
                    height: area_h,
                },
                card,
            )
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..area_h)
            .map(|y| {
                (0..area_w)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn column_ladder_boundaries() {
        assert_eq!(columns(149), 3);
        assert_eq!(columns(150), 6);
        assert_eq!(columns(99), 2);
        assert_eq!(columns(100), 3);
        assert_eq!(columns(35), 0);
        assert_eq!(columns(36), 2);
    }

    #[test]
    fn grid_rows_round_up_and_report_height() {
        assert_eq!(
            card_grid(150, 6),
            CardGrid {
                cols: 6,
                rows: 1,
                height: CARD_H
            }
        );
        assert_eq!(
            card_grid(120, 6),
            CardGrid {
                cols: 3,
                rows: 2,
                height: CARD_H * 2
            }
        );
        assert_eq!(card_grid(20, 6).cols, 0);
        assert_eq!(card_grid(150, 0).height, 0);
    }

    #[test]
    fn unavailable_value_suppresses_unit_bar_and_sub() {
        let t = Theme::mono();
        let card = Card::percent(&t, "KV CACHE", None, 0.75, 0.9)
            .sub(Some("40k / 90k tok".into()), t.dim)
            .spark(vec![1.0, 2.0]);
        let out = render(24, CARD_H, &card);
        assert!(out.contains("--"), "{out}");
        // No fabricated bar or sub-line for a value we do not have.
        assert!(!out.contains('█'), "{out}");
        assert!(!out.contains("40k"), "{out}");
    }

    #[test]
    fn present_value_renders_label_value_and_bar() {
        let t = Theme::mono();
        let card = Card::percent(&t, "KV CACHE", Some(0.42), 0.75, 0.9);
        let out = render(24, CARD_H, &card);
        assert!(out.contains("KV CACHE"), "{out}");
        assert!(out.contains("42.0%"), "{out}");
        // Mono theme draws the bar in ASCII.
        assert!(out.contains('#') || out.contains('.'), "{out}");
    }

    #[test]
    fn narrow_card_drops_unit_then_truncates_without_overflow() {
        let t = Theme::mono();
        let card = Card::count(&t, "GENERATION", Some(1234.5), Some("tok/s"));
        for w in 6..=30u16 {
            let out = render(w, CARD_H, &card);
            for line in out.lines() {
                assert_eq!(line.chars().count(), w as usize, "width {w}: {out}");
            }
        }
    }

    #[test]
    fn zero_sized_areas_do_not_panic() {
        let t = Theme::mono();
        let card = Card::count(&t, "RUNNING", Some(3.0), None);
        for (w, h) in [(0, 0), (1, 1), (2, 4), (24, 1), (24, 2)] {
            let _ = render(w.max(1), h.max(1), &card);
        }
    }
}

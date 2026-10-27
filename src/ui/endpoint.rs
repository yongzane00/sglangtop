//! Tabs 2…N: one endpoint in depth.
//!
//! Top to bottom: the head (identity, status, lifetime totals), then the two
//! instant instruments every panel mode shares — the KV cache bar and the
//! seats — then whatever `t` selects. The overview stacks tokens, TTFT dials
//! and the detail panels on a narrow (portrait) terminal, and sets tokens
//! beside the dials on a wide one. Space runs out in a fixed order: the
//! detail panels go first, then the dials shrink to one-line gauges, then
//! the instrument tiles become one-line gauges too.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::collections::BTreeMap;
use std::time::Instant;

use crate::app::{App, PanelMode};
use crate::metrics::normalize::{CuratedSeries, SeriesKey, hist};
use crate::state::{DerivedSeries, EndpointState, series_id};
use crate::ui::{dial, format, freshness_badge, kvbar, panels, seats, tokens};

/// From this width the overview goes side by side: instruments in one row,
/// tokens beside the dials, three detail panels across.
const WIDE: u16 = 150;
/// Narrower than this, or shorter than `TILE_MIN_REST` under the head, the
/// instruments are one-line gauges instead of bordered tiles.
const TILE_MIN_W: u16 = 60;
const TILE_MIN_REST: u16 = 20;

const TOKENS_MIN: u16 = 8;
/// In a portrait layout, height past this may go to the event band.
const TOKENS_MAX: u16 = 22;
/// Dials below this would be too small to read; below `DIALS_MIN + 4`
/// spare rows they become one-line gauges and tokens get the space.
const DIALS_MIN: u16 = 8;
const DIALS_MAX: u16 = 20;
const LOWER_MIN: u16 = 8;
const LOWER_MAX: u16 = 12;
const EXTRA_MIN: u16 = 6;
/// The event band stays small without a request log to fill it.
const EXTRA_MAX: u16 = 10;
const EXTRA_MAX_WITH_LOG: u16 = 30;
/// Borders + header + six rows: the percentiles table, whole.
const PERCENTILES_H: u16 = 9;
/// A wide layout caps the tokens/dials row here; the detail panels below
/// take the rest.
const MID_MAX: u16 = 26;
/// More data-parallel engines than this merge into one seat group.
const MAX_SEAT_GROUPS: usize = 8;

/// The head, the shared instruments, and `body` — what `t` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shell {
    head: Rect,
    kv: Rect,
    seats: Rect,
    body: Rect,
}

/// `seat_rows(inner_width)`: how many rows of boxes the seats need.
fn plan_shell(area: Rect, seat_rows: impl Fn(u16) -> u16) -> Shell {
    let head_h = 3.min(area.height);
    let [head, rest] =
        Layout::vertical([Constraint::Length(head_h), Constraint::Min(0)]).areas(area);

    let tiles = rest.width >= TILE_MIN_W && rest.height >= TILE_MIN_REST;
    if !tiles {
        let kv_h = rest.height.min(1);
        let seats_h = (rest.height - kv_h).min(1);
        let [kv, seats, body] = Layout::vertical([
            Constraint::Length(kv_h),
            Constraint::Length(seats_h),
            Constraint::Min(0),
        ])
        .areas(rest);
        return Shell {
            head,
            kv,
            seats,
            body,
        };
    }

    if rest.width >= WIDE {
        let kv_w = rest.width * 2 / 5;
        let row_h = (2 + seat_rows(rest.width - kv_w - 2)).max(3);
        let [row, body] =
            Layout::vertical([Constraint::Length(row_h), Constraint::Min(0)]).areas(rest);
        let [kv, seats] =
            Layout::horizontal([Constraint::Length(kv_w), Constraint::Min(0)]).areas(row);
        Shell {
            head,
            kv,
            seats,
            body,
        }
    } else {
        let seats_h = 2 + seat_rows(rest.width - 2);
        let [kv, seats, body] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(seats_h),
            Constraint::Min(0),
        ])
        .areas(rest);
        Shell {
            head,
            kv,
            seats,
            body,
        }
    }
}

/// The overview body. `dials` below `DIALS_MIN` rows means one-line gauges.
/// `side` (wide only) is the percentiles table under the dials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Overview {
    tokens: Rect,
    dials: Rect,
    side: Rect,
    lower: Rect,
    extra: Rect,
}

fn plan_overview(body: Rect, has_log: bool) -> Overview {
    let h = body.height;
    let need = TOKENS_MIN + DIALS_MIN;
    if h < need + 4 || body.width < TILE_MIN_W {
        // Compact: TTFT gauges in one row (two when narrow) over tokens.
        let gauge_h = if body.width >= 60 { 1 } else { 2 }.min(h);
        let [dials, tokens] =
            Layout::vertical([Constraint::Length(gauge_h), Constraint::Min(0)]).areas(body);
        return Overview {
            tokens,
            dials,
            ..Overview::default()
        };
    }

    let lower_h = if h >= need + LOWER_MIN {
        (h - need).min(LOWER_MAX)
    } else {
        0
    };

    if body.width >= WIDE {
        // Tokens beside the dials, the percentiles under the dials when
        // they fit (latency beside latency), other panels underneath.
        let mid_h = (h - lower_h).min(MID_MAX);
        let [mid, lower] =
            Layout::vertical([Constraint::Length(mid_h), Constraint::Min(0)]).areas(body);
        let [tokens, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(mid);
        let dials_h = dial::pair_rows_wanted(right.width).clamp(DIALS_MIN, mid_h);
        let side_h = if mid_h - dials_h >= PERCENTILES_H {
            mid_h - dials_h
        } else {
            0
        };
        let [dials, side] = Layout::vertical([
            Constraint::Length(mid_h - side_h),
            Constraint::Length(side_h),
        ])
        .areas(right);
        return Overview {
            tokens,
            dials,
            side,
            lower,
            extra: Rect::default(),
        };
    }

    // Portrait: tokens, dials, detail panels, then an event band when tall.
    // Spare height goes half to the dials (up to their full size), then to
    // the event band (small without a request log), the rest to tokens.
    let mut spare = h - need - lower_h;
    let dials_cap = dial::pair_rows_wanted(body.width).clamp(DIALS_MIN, DIALS_MAX);
    let dials_h = DIALS_MIN + (spare / 2).min(dials_cap - DIALS_MIN);
    spare -= dials_h - DIALS_MIN;
    let extra_cap = if has_log {
        EXTRA_MAX_WITH_LOG
    } else {
        EXTRA_MAX
    };
    let tokens_room = TOKENS_MAX - TOKENS_MIN;
    let extra_h = if lower_h > 0 && spare >= tokens_room + EXTRA_MIN {
        (spare - tokens_room).min(extra_cap)
    } else {
        0
    };
    let tokens_h = h - dials_h - lower_h - extra_h;
    let [tokens, dials, lower, extra] = Layout::vertical([
        Constraint::Length(tokens_h),
        Constraint::Length(dials_h),
        Constraint::Length(lower_h),
        Constraint::Length(extra_h),
    ])
    .areas(body);
    Overview {
        tokens,
        dials,
        side: Rect::default(),
        lower,
        extra,
    }
}

pub fn draw(frame: &mut Frame, app: &App, index: usize, area: Rect) {
    let Some(e) = app.endpoints.get(index) else {
        return;
    };
    let t = &app.theme;
    let kv = kv_reading(e);
    let seats = seats_reading(app, index, e);
    let shell = plan_shell(area, |w| seats.rows_needed(w));

    draw_head(frame, app, e, shell.head);
    kvbar::draw(frame, t, shell.kv, &kv);
    seats::draw(frame, t, shell.seats, &seats);

    let body = shell.body;
    match app.panel_mode {
        PanelMode::Overview => draw_overview(frame, app, index, e, body),
        // Tokens beside the live request log.
        PanelMode::Requests => {
            if body.width >= 100 {
                let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(body);
                tokens::draw(frame, t, left, e, app.config.history_window);
                draw_requests(frame, app, index, e, right);
            } else {
                // Too narrow to split: the request log is the point of this
                // mode, so it gets the space.
                draw_requests(frame, app, index, e, body);
            }
        }
        // The tables at exactly their height, the tokens panel below them.
        PanelMode::Tables => {
            let need = tables_height(app, e, body.width);
            if body.height >= need + TOKENS_MIN {
                let [tables, rest] =
                    Layout::vertical([Constraint::Length(need), Constraint::Min(0)]).areas(body);
                draw_tables(frame, app, e, tables);
                tokens::draw(frame, t, rest, e, app.config.history_window);
            } else {
                draw_tables(frame, app, e, body);
            }
        }
    }
}

fn draw_overview(frame: &mut Frame, app: &App, index: usize, e: &EndpointState, body: Rect) {
    let t = &app.theme;
    let has_log = app
        .config
        .endpoints
        .get(index)
        .is_some_and(|c| c.log_file.is_some());
    let o = plan_overview(body, has_log);
    tokens::draw(frame, t, o.tokens, e, app.config.history_window);

    let scale = dial::Scale::ttft(app.config.ttft_warn, app.config.ttft_crit);
    let window = format::brief_duration(app.config.history_window);
    dial::draw_pair(frame, t, o.dials, &ttft_readings(e), &scale, &window);

    let side_table = o.side.height > 0;
    if side_table {
        panels::draw_percentiles(frame, app, e, o.side);
    }
    if o.lower.height > 0 {
        draw_lower(frame, app, index, e, o.lower, has_log, side_table);
    }
    if o.extra.height > 0 {
        draw_extra(frame, app, index, e, o.extra, has_log);
    }
}

/// Detail panels, in priority order: percentiles (unless already beside
/// the dials), server/model info, then the live request log when one is
/// configured, else the events feed — as many as the width holds.
fn draw_lower(
    frame: &mut Frame,
    app: &App,
    index: usize,
    e: &EndpointState,
    area: Rect,
    has_log: bool,
    percentiles_placed: bool,
) {
    #[derive(Clone, Copy)]
    enum Panel {
        Percentiles,
        Info,
        Feed,
    }
    let order: &[Panel] = if percentiles_placed {
        &[Panel::Info, Panel::Feed]
    } else {
        &[Panel::Percentiles, Panel::Info, Panel::Feed]
    };
    let cols = if area.width >= WIDE {
        3
    } else if area.width >= 100 {
        2
    } else {
        1
    }
    .min(order.len());
    let cells = Layout::horizontal(vec![Constraint::Fill(1); cols]).split(area);
    for (panel, cell) in order.iter().zip(cells.iter()) {
        match panel {
            Panel::Percentiles => panels::draw_percentiles(frame, app, e, *cell),
            Panel::Info => panels::draw_info(frame, app, e, *cell),
            Panel::Feed if has_log => draw_requests(frame, app, index, e, *cell),
            Panel::Feed => panels::draw_events(frame, app, e, *cell),
        }
    }
}

/// The band a tall portrait terminal has room for: requests and events.
fn draw_extra(
    frame: &mut Frame,
    app: &App,
    index: usize,
    e: &EndpointState,
    area: Rect,
    has_log: bool,
) {
    if has_log && area.width >= 100 {
        let [left, right] = Layout::horizontal([Constraint::Fill(1); 2]).areas(area);
        draw_requests(frame, app, index, e, left);
        panels::draw_events(frame, app, e, right);
    } else if has_log {
        draw_requests(frame, app, index, e, area);
    } else {
        panels::draw_events(frame, app, e, area);
    }
}

fn kv_reading(e: &EndpointState) -> kvbar::Kv {
    let agg = e.aggregate();
    kvbar::Kv {
        frac: agg.kv_usage.map(|k| k.value()),
        tokens: kv_absolute(e),
        unweighted: agg.kv_usage.is_some_and(|k| !k.is_weighted()),
    }
}

/// Seats per data-parallel rank. The cap is the configured `max_running`
/// when set, else the server's own `--max-running-requests` from
/// `/server_info`, so unlike a vLLM dashboard the grid usually shows every
/// slot without anyone configuring anything. Each rank runs its own
/// scheduler, so the cap applies to each separately.
fn seats_reading(app: &App, index: usize, e: &EndpointState) -> seats::Seats {
    let configured = app.config.endpoints.get(index).and_then(|c| c.max_running);
    let cap = e
        .effective_max_running(configured)
        .filter(|m| *m > 0)
        .and_then(|m| u32::try_from(m).ok());
    let mut by_engine: BTreeMap<Option<String>, Option<f64>> = BTreeMap::new();
    if let Some(c) = &e.curated {
        for (key, s) in &c.series {
            let rank = key.dp_rank.as_ref().map(|r| r.to_string());
            let slot = by_engine.entry(rank).or_insert(None);
            if let Some(r) = s.running {
                *slot = Some(slot.unwrap_or(0.0) + r);
            }
        }
    }
    let whole = |v: f64| v.round().max(0.0) as u32;
    let multi = by_engine.len() > 1;
    let mut groups: Vec<seats::SeatGroup> = by_engine
        .into_iter()
        .map(|(engine, running)| seats::SeatGroup {
            label: multi.then(|| format!("dp{}", engine.as_deref().unwrap_or("?"))),
            running: running.map(whole),
            cap,
        })
        .collect();
    if groups.len() > MAX_SEAT_GROUPS {
        let n = groups.len() as u32;
        let running = groups
            .iter()
            .filter_map(|g| g.running)
            .fold(None, |acc, r| Some(acc.unwrap_or(0) + r));
        groups = vec![seats::SeatGroup {
            label: None,
            running,
            cap: cap.map(|c| c * n),
        }];
    }
    seats::Seats {
        groups,
        waiting: e.aggregate().waiting.map(whole),
    }
}

/// TTFT p50 and p95, each the worst across series (a mean would let one
/// slow engine hide), with each one's peak over the history window.
fn ttft_readings(e: &EndpointState) -> [dial::Reading<'static>; 2] {
    let worst = |pick: fn(&crate::metrics::histogram::WindowEstimate) -> Option<f64>| {
        e.derived
            .values()
            .filter_map(|d| d.estimates.get(hist::TTFT))
            .filter_map(pick)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |c| c.max(v)))
            })
    };
    let peak = |id: &str| {
        e.history
            .iter()
            .filter(|((_, sid), _)| *sid == id)
            .flat_map(|(_, ring)| ring.iter().map(|p| p.value))
            .filter(|v| v.is_finite())
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |c| c.max(v)))
            })
    };
    [
        dial::Reading {
            label: "p50",
            value: worst(|w| w.p50),
            peak: peak(series_id::TTFT_P50),
        },
        dial::Reading {
            label: "p95",
            value: worst(|w| w.p95),
            peak: peak(series_id::TTFT_P95),
        },
    ]
}

/// Absolute KV usage as `(used_tokens, capacity_tokens)`, only when EVERY
/// series reports both — otherwise the total would be a partial sum shown as
/// a whole. SGLang reports used tokens directly (`sglang:num_used_tokens`),
/// so unlike vLLM there is nothing to infer from a fraction.
fn kv_absolute(e: &EndpointState) -> Option<(f64, f64)> {
    let curated = e.curated.as_ref()?;
    let pairs: Vec<(f64, f64)> = curated
        .series
        .values()
        .filter_map(|s| {
            let cap = s.kv_cache_size_tokens?;
            let used = s
                .kv_used_tokens
                .or_else(|| s.kv_cache_usage.map(|u| u * cap))?;
            Some((used, cap))
        })
        .collect();
    if pairs.is_empty() || pairs.len() != curated.series.len() {
        return None;
    }
    Some((
        pairs.iter().map(|(u, _)| u).sum(),
        pairs.iter().map(|(_, c)| c).sum(),
    ))
}

/// Sum a per-series value across this endpoint's series.
fn series_sum(e: &EndpointState, get: fn(&CuratedSeries) -> Option<f64>) -> Option<f64> {
    e.curated.as_ref().and_then(|c| {
        c.series
            .values()
            .filter_map(get)
            .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v))
    })
}

/// Animated spinner glyph. The UI redraws at least every 500 ms, so this runs
/// at ~2 fps — enough to read as "alive". (Display-only wall-clock use.)
fn spinner(t: &crate::ui::theme::Theme) -> &'static str {
    let frames: &[&str] = if t.ascii() {
        &["|", "/", "-", "\\"]
    } else {
        &[
            "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}",
            "\u{2827}", "\u{2807}", "\u{280f}",
        ]
    };
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    frames[(ms / 250) as usize % frames.len()]
}

/// Model names are long; the head shows the last path segment.
fn short_model(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_string()
}

/// Lifetime totals and server-window rates: what this server has done since
/// it started (counters, so they reset when it restarts), plus the readouts
/// that have no instrument of their own (cache hit rate, speculative
/// decoding).
fn lifetime_spans(t: &crate::ui::theme::Theme, e: &EndpointState) -> Vec<Span<'static>> {
    let agg = e.aggregate();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let served = series_sum(e, |s| s.requests_total());
    if served.is_some() {
        spans.push(Span::styled("   served ", t.dim));
        spans.push(Span::styled(format::count(served), t.secondary));
    }
    let prompt_life = series_sum(e, |s| s.prompt_tokens_total());
    let gen_life = series_sum(e, |s| s.generation_tokens_total());
    if prompt_life.is_some() || gen_life.is_some() {
        spans.push(Span::styled("   tokens ", t.dim));
        spans.push(Span::styled(
            format!(
                "{} in / {} out",
                format::count(prompt_life),
                format::count(gen_life)
            ),
            t.secondary,
        ));
    }
    // The server's own rolling cache-hit window, not something we derive.
    if let Some(rate) = agg.cache_hit_rate {
        spans.push(Span::styled("   cache ", t.dim));
        spans.push(Span::styled(
            format!("{} (srv win)", format::percent(Some(rate))),
            t.secondary,
        ));
    }
    // Speculative decoding, when the server runs it.
    if let Some(curated) = &e.curated
        && let Some(s) = curated
            .series
            .values()
            .find(|s| s.spec_accept_rate.is_some() || s.spec_accept_length.is_some())
    {
        let mut parts: Vec<String> = Vec::new();
        if let Some(r) = s.spec_accept_rate {
            parts.push(format!("accept {}", format::percent(Some(r))));
        }
        if let Some(l) = s.spec_accept_length {
            parts.push(format!("{l:.1} tok/step"));
        }
        if !parts.is_empty() {
            spans.push(Span::styled("   spec ", t.dim));
            spans.push(Span::styled(parts.join(" · "), t.secondary));
        }
    }
    // HTTP totals come from the frontend instrumentation, covering all routes.
    if let Some(info) = e.curated.as_ref().map(|c| &c.info)
        && let Some(total) = info.http_total()
    {
        spans.push(Span::styled("   http ", t.dim));
        spans.push(Span::styled(format::count(Some(total)), t.secondary));
        let errors: f64 = info
            .http_by_status
            .iter()
            .filter(|(k, _)| k.starts_with('5'))
            .map(|(_, v)| v)
            .sum();
        if errors > 0.0 {
            spans.push(Span::styled(
                format!(" ({} 5xx)", format::count(Some(errors))),
                t.crit,
            ));
        }
    }
    spans
}

/// Two lines over a rule. Line 1: who, whether it answers, what it is doing.
/// Line 2: failures first, then what it serves, then lifetime totals. Each
/// line is ordered by importance, because a narrow terminal clips the end.
///
/// SGLang exports no `process_start_time_seconds`, so there is no uptime
/// here; it exports its PD role and speculative algorithm instead.
fn draw_head(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();
    let freshness = e.freshness(now, app.refresh_interval);
    let (badge, badge_style) = freshness_badge(app, freshness, e.healthy);
    let agg = e.aggregate();

    let mut line1 = vec![
        Span::styled(format!(" {} ", e.name), t.heading),
        Span::styled(badge, badge_style),
    ];
    // With no running-request gauge at all we know nothing, and "IDLE" would
    // be a claim.
    match agg.running {
        Some(r) if r > 0.0 => {
            line1.push(Span::styled(
                format!("  {} GENERATING", spinner(t)),
                t.value,
            ));
        }
        Some(_) => line1.push(Span::styled("  IDLE", t.dim)),
        None => line1.push(Span::styled(format!("  {}", format::NA), t.na)),
    }
    if e.restart_seen_at
        .is_some_and(|at| now.saturating_duration_since(at).as_secs() < 30)
    {
        line1.push(Span::styled("  RESTARTED", t.crit));
    }
    line1.push(Span::styled("   SGLang ", t.dim));
    line1.push(Span::styled(
        e.server_version
            .clone()
            .unwrap_or_else(|| format::NA.into()),
        t.secondary,
    ));
    if let Some(info) = &e.server_info {
        if let Some(algo) = info.speculative_algorithm.as_deref() {
            line1.push(Span::styled(format!("  spec:{algo}"), t.secondary));
        }
        if let Some(role) = info.pd_role() {
            line1.push(Span::styled(format!("  PD:{role}"), t.secondary));
        }
    }
    let loads_only = e
        .curated
        .as_ref()
        .is_some_and(|c| c.has(crate::metrics::loads::LOADS_ONLY));
    if loads_only {
        line1.push(Span::styled("  LOADS", t.warn));
    }
    // Which panel mode is showing; 't' cycles it.
    line1.push(Span::styled(
        format!("   [{}]", app.panel_mode.label()),
        t.dim,
    ));
    line1.push(Span::styled(format!("   {}", e.display_url), t.dim));

    let mut line2: Vec<Span> = vec![Span::raw(" ")];
    if let crate::state::ConnStatus::Failing { error, consecutive } = &e.status {
        // A 404 on /metrics from a server that told us metrics are off is a
        // configuration fact, not a fault — say what to do about it.
        let metrics_disabled = error.contains("/metrics returned HTTP 404")
            && e.server_info.as_ref().and_then(|i| i.enable_metrics) == Some(false);
        let shown = if metrics_disabled {
            "metrics disabled on server — relaunch with --enable-metrics"
        } else {
            error.as_str()
        };
        line2.push(Span::styled(
            format!("✗{consecutive} {}  ", format::truncate(shown, 60)),
            t.crit,
        ));
    } else if loads_only {
        line2.push(Span::styled(
            "gauges via /v1/loads — relaunch with --enable-metrics for rates & latencies  ",
            t.dim,
        ));
    }
    if e.parse_issue_count > 0 {
        line2.push(Span::styled(
            format!("parse-issues {}  ", e.parse_issue_count),
            t.warn,
        ));
    }
    // Which model, and how big is its context window? SGLang exports the
    // context length as a metric as well as in /v1/models.
    let model_name = e
        .served_models
        .first()
        .map(|m| m.id.clone())
        .or_else(|| agg.models.first().cloned());
    if let Some(first) = model_name {
        let mut models = short_model(&first);
        if e.served_models.len() > 1 {
            models.push_str(&format!(" +{}", e.served_models.len() - 1));
        }
        line2.push(Span::styled(format::truncate(&models, 40), t.secondary));
        let ctx = e
            .served_models
            .iter()
            .find_map(|m| m.max_model_len.map(|c| c as f64))
            .or_else(|| series_sum(e, |s| s.context_len));
        if let Some(ctx) = ctx {
            line2.push(Span::styled(
                format!(" · ctx {}", format::count(Some(ctx))),
                t.dim,
            ));
        }
    }
    line2.extend(lifetime_spans(t, e));

    frame.render_widget(
        Paragraph::new(vec![Line::from(line1), Line::from(line2)])
            .block(Block::new().borders(Borders::BOTTOM).border_style(t.dim)),
        area,
    );
}

/// ACTIVITY and LATENCY need ~55 and ~65 columns: side by side from here,
/// stacked below it.
const TABLES_SIDE_BY_SIDE: u16 = 130;

/// Rows the tables view needs at `width`, so it takes no more than that.
fn tables_height(app: &App, e: &EndpointState, width: u16) -> u16 {
    let activity = activity_lines(app, e).len() as u16;
    let latency = latency_rows(e) + 1;
    if width >= TABLES_SIDE_BY_SIDE {
        activity.max(latency)
    } else {
        activity + 1 + latency
    }
}

fn draw_tables(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    if area.width >= TABLES_SIDE_BY_SIDE {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)])
                .areas(area);
        frame.render_widget(Paragraph::new(activity_lines(app, e)), left);
        draw_latency(frame, app, e, right);
    } else {
        let lines = activity_lines(app, e);
        let [top, _, bottom] = Layout::vertical([
            Constraint::Length(lines.len() as u16),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(Paragraph::new(lines), top);
        draw_latency(frame, app, e, bottom);
    }
}

/// Activity and cache metrics, one section per series.
fn activity_lines<'a>(app: &'a App, e: &'a EndpointState) -> Vec<Line<'a>> {
    let t = &app.theme;
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let mut lines: Vec<Line> = Vec::new();

    let Some(curated) = &e.curated else {
        lines.push(Line::from(Span::styled(" no data yet", t.na)));
        return lines;
    };

    for (key, series) in &curated.series {
        let d = e.derived.get(key);
        push_series_activity(
            &mut lines,
            app,
            key,
            series,
            d,
            curated.series.len() > 1,
            ascii,
        );
    }
    lines
}

/// Latency table rows, header excluded: eight per series, plus a heading
/// per series when there are several.
fn latency_rows(e: &EndpointState) -> u16 {
    let Some(curated) = &e.curated else {
        return 0;
    };
    let per = 8 + u16::from(curated.series.len() > 1);
    curated
        .series
        .keys()
        .filter(|k| e.derived.contains_key(*k))
        .count() as u16
        * per
}

fn push_series_activity(
    lines: &mut Vec<Line>,
    app: &App,
    key: &SeriesKey,
    s: &CuratedSeries,
    d: Option<&DerivedSeries>,
    multi: bool,
    ascii: bool,
) {
    let t = &app.theme;
    if multi {
        lines.push(Line::from(Span::styled(
            format!(" ▸ {}", key.display()),
            t.heading,
        )));
    } else {
        lines.push(Line::from(Span::styled(" ACTIVITY", t.heading)));
    }

    let kv = |label: &str, value: String, style| {
        Line::from(vec![
            // Width fits the longest label, "generation tokens/s" (19).
            Span::styled(format!("   {label:<20}"), t.dim),
            Span::styled(value, style),
        ])
    };

    // With one series, running/waiting/KV/token rates already live in the
    // pulse strip above — repeating them here is noise. Multi-series
    // (multi-model / data-parallel) endpoints keep the full per-series rows
    // because the pulse strip only shows aggregates.
    if multi {
        let wait_style = if s.waiting.unwrap_or(0.0) > 0.0 {
            t.warn
        } else {
            t.value
        };
        lines.push(kv("running", format::count(s.running), t.value));
        let mut waiting_text = format::count(s.waiting);
        if !s.waiting_extras.is_empty() {
            let extras: Vec<String> = s
                .waiting_extras
                .iter()
                .filter(|(_, v)| **v > 0.0)
                .map(|(k, v)| format!("{k}:{}", format::count(Some(*v))))
                .collect();
            if !extras.is_empty() {
                waiting_text = format!("{waiting_text} ({})", extras.join(" "));
            }
        }
        lines.push(kv("waiting", waiting_text, wait_style));

        match s.kv_cache_usage {
            Some(u) => {
                let style = t.by_level(u, 0.75, 0.9);
                let mut text =
                    format!("{} {}", format::bar(u, 20, ascii), format::percent(Some(u)));
                if let Some(cap) = s.kv_cache_size_tokens {
                    text.push_str(&format!("  of {} tok", format::count(Some(cap))));
                }
                lines.push(kv("KV cache", text, style));
            }
            None => lines.push(kv("KV cache", format::NA.into(), t.na)),
        }

        lines.push(kv(
            "prompt tokens/s",
            format::count(d.and_then(|d| d.prompt_tps)),
            t.value,
        ));
        lines.push(kv(
            "generation tokens/s",
            format::count(d.and_then(|d| d.generation_tps)),
            t.value,
        ));
        // Per-series lifetime token totals; the pulse strip only has the
        // endpoint-wide sum.
        lines.push(kv(
            "tokens (life)",
            format!(
                "{} in  {} out",
                format::count(s.prompt_tokens_total()),
                format::count(s.generation_tokens_total())
            ),
            t.secondary,
        ));
    } else if !s.waiting_extras.is_empty() {
        // Auxiliary queues (grammar/paused/retracted) have no home in the
        // pulse strip; show them when any are non-zero.
        let extras: Vec<String> = s
            .waiting_extras
            .iter()
            .filter(|(_, v)| **v > 0.0)
            .map(|(k, v)| format!("{k}:{}", format::count(Some(*v))))
            .collect();
        if !extras.is_empty() {
            lines.push(kv("aux queues", extras.join(" "), t.warn));
        }
    }
    lines.push(kv(
        "completions/s",
        format::count(d.and_then(|d| d.request_rate)),
        t.value,
    ));

    // Lifetime completion split by streaming mode (SGLang counts finished
    // requests per is_streaming; aborts live in their own counter).
    if !s.requests_by_stream.is_empty() {
        let text: Vec<String> = s
            .requests_by_stream
            .iter()
            .map(|(k, v)| {
                let label = match k.as_str() {
                    "true" => "streaming",
                    "false" => "non-streaming",
                    other => other,
                };
                format!("{label}:{}", format::count(Some(*v)))
            })
            .collect();
        lines.push(kv("finished (life)", text.join(" "), t.secondary));
    }

    let abort_delta = d.and_then(|d| d.abort_delta);
    lines.push(kv(
        "aborts",
        format!(
            "{} new  {} life",
            format::count(abort_delta),
            format::count(s.aborted)
        ),
        if abort_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.text
        },
    ));

    let retr_delta = d.and_then(|d| d.retraction_delta);
    lines.push(kv(
        "retractions",
        format!(
            "{} new  {} life",
            format::count(retr_delta),
            format::count(s.retracted)
        ),
        if retr_delta.unwrap_or(0.0) > 0.0 {
            t.crit
        } else {
            t.text
        },
    ));

    // Two cache readings with different windows: the server's own rolling
    // hit rate, and our scrape-window share of prompt tokens served from
    // cache (numerator/denominator both attributed at request finish).
    lines.push(kv(
        "cache hit (srv win)",
        format::percent(s.cache_hit_rate_recent),
        t.secondary,
    ));
    let share = d.and_then(|d| d.cached_token_share_window);
    if share.is_some() || s.cached_tokens_total().is_some() {
        lines.push(kv("cached tok share", format::percent(share), t.secondary));
    }

    // Speculative decoding (absent unless the server runs a spec algorithm).
    if s.spec_accept_rate.is_some() || s.spec_accept_length.is_some() {
        lines.push(kv(
            "spec decode",
            format!(
                "accept {}  {} tok/step",
                format::percent(s.spec_accept_rate),
                format::count(s.spec_accept_length)
            ),
            t.secondary,
        ));
    }

    // Hybrid-model pool detail (SWA/Mamba models expose per-pool usage).
    if !s.kv_usage_detail.is_empty() {
        let text: Vec<String> = s
            .kv_usage_detail
            .iter()
            .map(|(k, v)| format!("{k}:{}", format::percent(Some(*v))))
            .collect();
        lines.push(kv("pool usage", text.join(" "), t.secondary));
    }

    // PD-disaggregation queues, role-appropriate.
    let pd_pairs: [(&str, Option<f64>); 4] = [
        ("bootstrap", s.pd_bootstrap_queue),
        ("inflight", s.pd_inflight_queue),
        ("prealloc", s.pd_prealloc_queue),
        ("transfer", s.pd_transfer_queue),
    ];
    let pd: Vec<String> = pd_pairs
        .iter()
        .filter_map(|(k, v)| v.map(|v| format!("{k}:{}", format::count(Some(v)))))
        .collect();
    if !pd.is_empty() {
        lines.push(kv("PD queues", pd.join(" "), t.secondary));
    }
    lines.push(Line::default());
}

/// Right column: latency percentile table per series.
fn draw_latency(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let Some(curated) = &e.curated else {
        return;
    };

    // The four core request latencies always render (`--` when quiet); the
    // per-stage rows below appear only when that stage has observations —
    // SGLang has no per-request prefill/decode-time histograms, and a fake
    // `--` row for a stage the server never reports would imply it exists.
    const ROWS: [(&str, &str); 4] = [
        (hist::TTFT, "TTFT"),
        (hist::INTER_TOKEN_LATENCY, "inter-token"),
        (hist::E2E_LATENCY, "e2e latency"),
        (hist::QUEUE_TIME, "queue time"),
    ];
    const STAGE_ROWS: [(&str, &str); 8] = [
        (hist::STAGE_REQUEST_PROCESS, "req process"),
        (hist::STAGE_PREFILL_FORWARD, "prefill fwd"),
        (hist::STAGE_CHUNKED_PREFILL, "chunked prefill"),
        (hist::STAGE_PREFILL_BOOTSTRAP, "bootstrap (PD)"),
        (hist::STAGE_PREFILL_TRANSFER_KV, "kv send (PD)"),
        (hist::KV_TRANSFER_LATENCY, "kv transfer (PD)"),
        (hist::STAGE_DECODE_WAITING, "decode wait (PD)"),
        (hist::STAGE_DECODE_PREPARE, "decode prep (PD)"),
    ];

    let mut rows: Vec<Row> = Vec::new();
    let window = app.config.percentile_window.as_secs();
    for key in curated.series.keys() {
        let Some(d) = e.derived.get(key) else {
            continue;
        };
        if curated.series.len() > 1 {
            rows.push(Row::new(vec![Cell::from(Span::styled(
                format!("▸ {}", key.display()),
                t.heading,
            ))]));
        }
        let stage_rows = STAGE_ROWS
            .iter()
            .filter(|(id, _)| d.estimates.contains_key(id))
            .copied();
        for (id, label) in ROWS.iter().copied().chain(stage_rows) {
            let est = d.estimates.get(id);
            let cell = |v: Option<f64>| {
                Cell::from(Span::styled(
                    format::seconds(v),
                    if v.is_some() { t.value } else { t.na },
                ))
            };
            let obs = est.map(|e| e.observations).unwrap_or(0.0);
            rows.push(Row::new(vec![
                Cell::from(Span::styled(format!("  {label}"), t.dim)),
                cell(est.and_then(|e| e.p50)),
                cell(est.and_then(|e| e.p95)),
                cell(est.and_then(|e| e.p99)),
                cell(est.and_then(|e| e.mean)),
                Cell::from(Span::styled(
                    if est.is_some() {
                        format::count(Some(obs))
                    } else {
                        format::NA.into()
                    },
                    t.dim,
                )),
            ]));
        }
        // Tokens-per-request distributions, same estimator. The uncached
        // row (prompt tokens that missed every cache tier) appears only
        // when the server reports it.
        let uncached = [(hist::UNCACHED_PROMPT_TOKENS_PER_REQ, "uncached tok/req")]
            .into_iter()
            .filter(|(id, _)| d.estimates.contains_key(id));
        for (id, label) in [
            (hist::PROMPT_TOKENS_PER_REQ, "prompt tok/req"),
            (hist::GENERATION_TOKENS_PER_REQ, "gen tok/req"),
        ]
        .into_iter()
        .chain(uncached)
        {
            let est = d.estimates.get(id);
            let cell = |v: Option<f64>| {
                Cell::from(Span::styled(
                    format::count(v),
                    if v.is_some() { t.secondary } else { t.na },
                ))
            };
            rows.push(Row::new(vec![
                Cell::from(Span::styled(format!("  {label}"), t.dim)),
                cell(est.and_then(|e| e.p50)),
                cell(est.and_then(|e| e.p95)),
                cell(est.and_then(|e| e.p99)),
                cell(est.and_then(|e| e.mean)),
                Cell::from(Span::raw("")),
            ]));
        }
    }

    let header = Row::new(
        [
            format!("LATENCY (~{window}s est)"),
            "p50".into(),
            "p95".into(),
            "p99".into(),
            "mean".into(),
            "obs".into(),
        ]
        .into_iter()
        .map(|h: String| Cell::from(Span::styled(h, t.heading))),
    );
    let widths = [
        Constraint::Length(22),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(6),
    ];
    frame.render_widget(
        Table::new(rows, widths).header(header).column_spacing(1),
        area,
    );
}

/// Recent requests from the log tailer.
fn draw_requests(frame: &mut Frame, app: &App, index: usize, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();

    let mut title_spans = vec![Span::styled(" requests ", t.heading)];
    if !e.requests.is_empty() {
        title_spans.push(Span::styled(format!("{} ", e.requests.len()), t.dim));
    }
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(t.dim)
        .title(Line::from(title_spans));

    let log_file = app
        .config
        .endpoints
        .get(index)
        .and_then(|c| c.log_file.as_ref());
    let empty_note = |text: String, style| {
        Paragraph::new(Line::from(Span::styled(text, style)))
            .block(block.clone())
            .wrap(ratatui::widgets::Wrap { trim: false })
    };

    // Empty states, most fundamental first.
    let Some(path) = log_file else {
        frame.render_widget(
            empty_note(
                " no log configured — set log_file for this endpoint in the \
                 config file (see examples/config.toml)"
                    .into(),
                t.na,
            ),
            area,
        );
        return;
    };
    if e.requests.tail_status == Some(crate::logtail::TailStatus::FileMissing) {
        frame.render_widget(
            empty_note(format!(" log file not found: {}", path.display()), t.warn),
            area,
        );
        return;
    }
    if e.requests.is_empty() {
        frame.render_widget(
            empty_note(
                " log tailing is running, but SGLang's log grammar is not \
                 parsed yet — this pane is reserved for a future \
                 scheduler-activity ticker"
                    .into(),
                t.na,
            ),
            area,
        );
        return;
    }

    let header = Row::new(
        ["age", "status", "prompt", "req id", "max_tok"]
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, t.heading))),
    );
    // Rows bounded by what fits: newest first, no point building all 200.
    let visible = area.height.saturating_sub(3) as usize; // borders + header
    let rows: Vec<Row> = e
        .requests
        .iter_newest_first()
        .take(visible.max(1))
        .map(|entry| {
            let finished = matches!(
                entry.status,
                crate::state::requests::RequestStatus::Finished { .. }
            );
            let row_style = if finished { t.dim } else { t.text };
            let age = format::brief_duration(now.saturating_duration_since(entry.seen_at));
            // The finish reason is the one thing the log tells us about how a
            // request ended; `length` and `abort` read very differently from
            // `stop`.
            let (status_text, status_style) = match &entry.status {
                crate::state::requests::RequestStatus::Generating => {
                    ("● running".to_string(), t.value)
                }
                crate::state::requests::RequestStatus::Finished { reason } => {
                    match reason.as_deref() {
                        Some("stop") => ("○ stop".to_string(), t.dim),
                        Some(other) => (format!("○ {other}"), t.warn),
                        None => ("○ done".to_string(), t.dim),
                    }
                }
            };
            let prompt = match &entry.prompt_preview {
                Some(p) => Span::styled(p.clone(), row_style),
                None => Span::styled(format::NA, t.na),
            };
            Row::new(vec![
                Cell::from(Span::styled(age, t.dim)),
                Cell::from(Span::styled(status_text, status_style)),
                Cell::from(prompt),
                Cell::from(Span::styled(format::truncate(&entry.id, 14), t.secondary)),
                Cell::from(Span::styled(
                    entry
                        .max_tokens
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| format::NA.into()),
                    t.value,
                )),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(5),
        Constraint::Length(9),
        Constraint::Min(10),
        Constraint::Length(14),
        Constraint::Length(7),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .block(block),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(r: Rect, area: Rect) -> bool {
        r.is_empty()
            || (r.x >= area.x
                && r.y >= area.y
                && r.right() <= area.right()
                && r.bottom() <= area.bottom())
    }

    fn disjoint(parts: &[Rect]) -> bool {
        let live: Vec<&Rect> = parts.iter().filter(|r| !r.is_empty()).collect();
        live.iter()
            .enumerate()
            .all(|(i, a)| live[i + 1..].iter().all(|b| !a.intersects(**b)))
    }

    #[test]
    fn every_layout_stays_inside_its_area_without_overlap() {
        // A coarse sweep plus every threshold in the ladder: the solver is
        // too slow for every size, and off-by-ones live at the boundaries.
        let widths: Vec<u16> = (1u16..=230)
            .step_by(11)
            .chain([59, 60, 61, 99, 100, 101, 149, 150, 151])
            .collect();
        let heights: Vec<u16> = (1u16..=110)
            .step_by(3)
            .chain([18, 19, 20, 21, 22, 23, 24, 35, 36, 37])
            .collect();
        for &w in &widths {
            for &h in &heights {
                let area = Rect::new(0, 0, w, h);
                for seat_rows in [1u16, 8] {
                    let s = plan_shell(area, |_| seat_rows);
                    let parts = [s.head, s.kv, s.seats, s.body];
                    assert!(parts.iter().all(|r| inside(*r, area)), "{w}x{h}: {s:?}");
                    assert!(disjoint(&parts), "{w}x{h}: {s:?}");

                    for has_log in [false, true] {
                        let o = plan_overview(s.body, has_log);
                        let parts = [o.tokens, o.dials, o.side, o.lower, o.extra];
                        assert!(parts.iter().all(|r| inside(*r, s.body)), "{w}x{h}: {o:?}");
                        assert!(disjoint(&parts), "{w}x{h}: {o:?}");
                        // Dials and tokens either both get readable sizes or
                        // the dials collapse to gauge rows over the tokens.
                        if o.dials.height >= DIALS_MIN {
                            assert!(o.tokens.height >= TOKENS_MIN, "{w}x{h}: {o:?}");
                        } else {
                            assert!(o.dials.height <= 2, "{w}x{h}: {o:?}");
                        }
                    }
                }
            }
        }
    }

    /// The sizes this layout is designed around, as the endpoint view
    /// receives them (the app's header and footer rows already taken off).
    #[test]
    fn the_ladder_at_its_design_sizes() {
        let one_row = |_| 1;

        // Portrait, half height: every instrument, tokens over dials.
        let s = plan_shell(Rect::new(0, 0, 110, 42), one_row);
        assert_eq!((s.head.height, s.kv.height, s.seats.height), (3, 3, 3));
        let o = plan_overview(s.body, false);
        assert_eq!(
            (
                o.tokens.height,
                o.dials.height,
                o.lower.height,
                o.extra.height
            ),
            (11, 10, 12, 0)
        );

        // Portrait, full height: full-size dials, a small event band, and
        // the rest to tokens; a request log earns a bigger band.
        let s = plan_shell(Rect::new(0, 0, 110, 93), one_row);
        let o = plan_overview(s.body, false);
        assert_eq!(
            (
                o.tokens.height,
                o.dials.height,
                o.lower.height,
                o.extra.height
            ),
            (42, 20, 12, 10)
        );
        let o = plan_overview(s.body, true);
        assert_eq!((o.tokens.height, o.extra.height), (22, 30));

        // Wide: instruments side by side; tokens beside the dials with the
        // percentiles tucked under them.
        let s = plan_shell(Rect::new(0, 0, 200, 48), one_row);
        assert_eq!(s.kv.y, s.seats.y);
        assert_eq!((s.kv.width, s.seats.width), (80, 120));
        let o = plan_overview(s.body, false);
        assert_eq!(o.tokens.y, o.dials.y);
        assert_eq!((o.dials.height, o.side.height), (17, 9));
        assert_eq!(o.lower.height, 16);

        // Small: one-line instruments and one row of TTFT gauges.
        let s = plan_shell(Rect::new(0, 0, 80, 22), one_row);
        assert_eq!((s.kv.height, s.seats.height), (1, 1));
        let o = plan_overview(s.body, false);
        assert_eq!((o.dials.height, o.tokens.height), (1, 16));
    }
}

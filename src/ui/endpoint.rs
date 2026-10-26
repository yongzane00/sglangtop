//! Tabs 2…N: one endpoint in depth.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::{App, PanelMode};
use crate::metrics::normalize::{CuratedSeries, SeriesKey, hist};
use crate::state::{DerivedSeries, EndpointState};
use crate::ui::{cards, charts, format, freshness_badge, panels};

/// The overview detail band needs this many rows to be worth drawing.
const MIN_LOWER_H: u16 = 8;
const MAX_LOWER_H: u16 = 12;

pub fn draw(frame: &mut Frame, app: &App, index: usize, area: Rect) {
    let Some(e) = app.endpoints.get(index) else {
        return;
    };
    let cards = endpoint_cards(app, index, e);
    let grid = cards::card_grid(area.width, cards.len());
    let head_h = 3.min(area.height);
    let avail = area.height.saturating_sub(head_h);
    // The card band never takes more than a third of the view, so the panels
    // below always keep room.
    let max_cards_h = (avail / 3 / cards::CARD_H) * cards::CARD_H;
    let cards_h = grid.height.min(max_cards_h);
    let life_h = u16::from(cards_h > 0 && avail > cards_h + 1);

    let [head, cards_area, life, body] = Layout::vertical([
        Constraint::Length(head_h),
        Constraint::Length(cards_h),
        Constraint::Length(life_h),
        Constraint::Min(0),
    ])
    .areas(area);

    draw_head(frame, app, e, head);
    if cards_h > 0 {
        cards::draw_card_row(frame, &app.theme, cards_area, &cards);
    }
    if life_h > 0 {
        draw_lifetime(frame, app, e, life);
    }

    match app.panel_mode {
        // The six live charts.
        PanelMode::Overview => draw_chart_band(frame, app, index, e, body),
        // Token-rate charts beside the live request log.
        PanelMode::Requests => {
            if body.width >= 100 {
                let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(body);
                draw_rate_charts(frame, app, e, left);
                draw_requests(frame, app, index, e, right);
            } else {
                // Too narrow to split: the request log is the point of this
                // mode, so it gets the space.
                draw_requests(frame, app, index, e, body);
            }
        }
        // The percentile tables, with trend charts below when roomy.
        PanelMode::Tables => {
            if body.height >= 14 {
                let [tables, charts] =
                    Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)])
                        .areas(body);
                draw_tables(frame, app, e, tables);
                draw_trends(frame, app, e, charts);
            } else {
                draw_tables(frame, app, e, body);
            }
        }
    }
}

/// The overview band: up to six charts, 3 columns when wide, 2 at 100+, 1
/// below. Rows come from the height, so a short terminal shows fewer.
fn draw_chart_band(frame: &mut Frame, app: &App, index: usize, e: &EndpointState, area: Rect) {
    let cols = if area.width >= 150 {
        3
    } else if area.width >= 100 {
        2
    } else {
        1
    };
    let specs = chart_specs(e);
    let chart_rows = specs.len().div_ceil(cols) as u16;

    // The detail band never costs a chart row: it appears only with height
    // left over after every chart row has its minimum.
    let needed_for_charts = chart_rows * charts::MIN_CHART_H;
    let lower_h = if area.height >= needed_for_charts + MIN_LOWER_H {
        (area.height - needed_for_charts).clamp(MIN_LOWER_H, MAX_LOWER_H)
    } else {
        0
    };
    let [charts_area, lower] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(lower_h)]).areas(area);

    let cells = charts::grid_cells(charts_area, cols, specs.len());
    for (spec, cell) in specs.iter().zip(cells) {
        charts::draw_single_chart(frame, app, cell, e, spec);
    }
    if lower_h > 0 {
        draw_lower(frame, app, index, e, lower);
    }
}

/// Detail band under the overview charts: percentiles, server/model info,
/// and either the live request log (when one is configured) or the events
/// sglangtop has observed.
fn draw_lower(frame: &mut Frame, app: &App, index: usize, e: &EndpointState, area: Rect) {
    let cols = if area.width >= 150 {
        3
    } else if area.width >= 110 {
        2
    } else {
        1
    };
    let cells = Layout::horizontal(vec![Constraint::Fill(1); cols]).split(area);
    panels::draw_percentiles(frame, app, e, cells[0]);
    if cols >= 2 {
        panels::draw_info(frame, app, e, cells[1]);
    }
    if cols >= 3 {
        let has_log = app
            .config
            .endpoints
            .get(index)
            .is_some_and(|c| c.log_file.is_some());
        if has_log {
            draw_requests(frame, app, index, e, cells[2]);
        } else {
            panels::draw_events(frame, app, e, cells[2]);
        }
    }
}

/// The six overview charts, in priority (= drop) order. Aborts, retractions
/// and e2e latency stay in the fleet grid and the tables view: an interval
/// delta and a per-second rate must not share a y-axis.
fn chart_specs(e: &EndpointState) -> Vec<charts::ChartSpec<'static>> {
    use crate::state::series_id as sid;
    let agg = e.aggregate();
    vec![
        charts::ChartSpec {
            title: "tokens/s",
            headline: None,
            series: &[
                charts::SeriesSpec {
                    id: sid::PROMPT_TPS,
                    label: "in",
                },
                charts::SeriesSpec {
                    id: sid::GENERATION_TPS,
                    label: "out",
                },
            ],
            kind: charts::ValueKind::Count,
        },
        charts::ChartSpec {
            title: "requests",
            headline: None,
            series: &[
                charts::SeriesSpec {
                    id: sid::RUNNING,
                    label: "run",
                },
                charts::SeriesSpec {
                    id: sid::WAITING,
                    label: "wait",
                },
            ],
            kind: charts::ValueKind::Count,
        },
        charts::ChartSpec {
            title: "KV token usage",
            headline: Some(format::percent(agg.kv_usage.map(|k| k.value()))),
            series: &[charts::SeriesSpec {
                id: sid::KV_USAGE,
                label: "",
            }],
            kind: charts::ValueKind::Fraction,
        },
        charts::ChartSpec {
            title: "TTFT",
            headline: None,
            series: &[
                charts::SeriesSpec {
                    id: sid::TTFT_P95,
                    label: "p95",
                },
                charts::SeriesSpec {
                    id: sid::TTFT_P50,
                    label: "p50",
                },
            ],
            kind: charts::ValueKind::Seconds,
        },
        // SGLang reports its own rolling prefix-cache hit rate; charting it
        // shows whether reuse is holding up under load.
        charts::ChartSpec {
            title: "cache hit (srv win)",
            headline: Some(format::percent(agg.cache_hit_rate)),
            series: &[charts::SeriesSpec {
                id: sid::CACHE_HIT_RATE,
                label: "",
            }],
            kind: charts::ValueKind::Fraction,
        },
        charts::ChartSpec {
            title: "completions/s",
            headline: Some(format::count(agg.request_rate)),
            series: &[charts::SeriesSpec {
                id: sid::REQUEST_RATE,
                label: "",
            }],
            kind: charts::ValueKind::Count,
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

/// Recent ring values for a sparkline — ONLY for single-series endpoints. On
/// a PD-disaggregated or DP-sharded endpoint, one series' shape is not the
/// endpoint's.
fn spark_values(e: &EndpointState, id: &'static str, n: usize) -> Vec<f64> {
    if e.curated.as_ref().map(|c| c.series.len()) != Some(1) {
        return Vec::new();
    }
    e.history
        .iter()
        .find(|((_, sid), _)| *sid == id)
        .map(|(_, ring)| ring.tail_values(n))
        .unwrap_or_default()
}

/// The six cards, in priority (= drop) order.
///
/// Note what is absent compared with a vLLM dashboard: SGLang exports no
/// `process_start_time_seconds`, so there is no uptime to show. It exports
/// rather more instead — GPU memory in GB, and the context window as a
/// metric — which is why the KV and MODEL cards carry more than a fraction.
fn endpoint_cards(app: &App, index: usize, e: &EndpointState) -> Vec<cards::Card> {
    let t = &app.theme;
    let agg = e.aggregate();

    // 1. Is it working? With no running-request gauge at all we know nothing,
    //    and "IDLE" would be a claim, so the card stays `--`.
    let generating = agg.running.is_some_and(|r| r > 0.0);
    let status_value = agg.running.map(|_| {
        if generating {
            format!("{} GENERATING", spinner(t))
        } else {
            "IDLE".to_string()
        }
    });
    let pd_role = e
        .server_info
        .as_ref()
        .and_then(|i| i.pd_role())
        .map(|r| format!("{r} role"));
    let status = cards::Card::text(t, "STATUS", status_value)
        .style(if generating { t.value } else { t.dim })
        .sub(pd_role, t.secondary);

    // 2. Which model, and how big is its context window? SGLang exports the
    //    context length as a metric as well as in /v1/models.
    let model_name = e
        .served_models
        .first()
        .map(|m| m.id.clone())
        .or_else(|| agg.models.first().cloned())
        .map(|m| short_model(&m));
    let ctx = e
        .served_models
        .iter()
        .find_map(|m| m.max_model_len.map(|c| c as f64))
        .or_else(|| series_sum(e, |s| s.context_len));
    let model = cards::Card::text(t, "MODEL", model_name).sub(
        ctx.map(|c| format!("ctx {}", format::count(Some(c)))),
        t.dim,
    );

    // 3. KV cache: fraction, plus absolute tokens AND gigabytes — SGLang
    //    reports `kv_cache_memory_usage_gb` directly.
    let kv_frac = agg.kv_usage.map(|k| k.value());
    let kv_gb = series_sum(e, |s| s.kv_cache_memory_gb);
    let kv_sub = match (kv_absolute(e), kv_gb) {
        (Some((used, cap)), Some(gb)) => Some(format!(
            "{} / {} tok · {:.1} GB",
            format::count(Some(used)),
            format::count(Some(cap)),
            gb
        )),
        (Some((used, cap)), None) => Some(format!(
            "{} / {} tok",
            format::count(Some(used)),
            format::count(Some(cap))
        )),
        (None, Some(gb)) => Some(format!("{gb:.1} GB")),
        (None, None) if agg.kv_usage.is_some_and(|k| !k.is_weighted()) => {
            Some("unweighted".to_string())
        }
        (None, None) => None,
    };
    let kv = cards::Card::percent(t, "KV CACHE", kv_frac, 0.75, 0.9).sub(kv_sub, t.dim);

    // 4. Scheduler occupancy. `max_running` comes from the config when set,
    //    else from the server's own `/server_info` — so unlike vLLM the n/max
    //    bar usually appears without anyone configuring anything.
    let configured = app.config.endpoints.get(index).and_then(|c| c.max_running);
    let max_running = e.effective_max_running(configured).filter(|m| *m > 0);
    let waiting = agg.waiting.unwrap_or(0.0);
    let requests = match (agg.running, max_running) {
        (Some(run), Some(max)) => {
            let frac = run / max as f64;
            cards::Card::text(
                t,
                "RUNNING",
                Some(format!("{}/{}", format::count(Some(run)), max)),
            )
            .style(t.by_level(frac, 0.75, 0.95))
            .bar(Some(frac), 0.75, 0.95)
        }
        (run, _) => cards::Card::count(t, "RUNNING", run, None),
    }
    .sub(
        agg.waiting
            .map(|w| format!("{} waiting", format::count(Some(w)))),
        if waiting > 0.0 { t.warn } else { t.dim },
    );

    // 5. Throughput. A `~` marks the server's own gauge rather than a
    //    counter-derived rate.
    let gen_value = agg.generation_tps.map(|v| {
        if agg.generation_tps_is_estimate {
            format!("~{}", format::count(Some(v)))
        } else {
            format::count(Some(v))
        }
    });
    let tokens = cards::Card::text(t, "GENERATION", gen_value)
        .sub(
            agg.prompt_tps
                .map(|p| format!("in {} tok/s", format::count(Some(p)))),
            t.dim,
        )
        .spark(spark_values(e, crate::state::series_id::GENERATION_TPS, 16));

    // 6. Latency: worst p95 across series, with the median beside it.
    let ttft_p50 = e
        .derived
        .values()
        .filter_map(|d| d.estimates.get(hist::TTFT))
        .filter_map(|est| est.p50)
        .fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |cur: f64| cur.max(v)))
        });
    let latency = cards::Card::seconds(t, "TTFT p95", agg.worst_ttft_p95).sub(
        ttft_p50.map(|p| format!("p50 {}", format::seconds(Some(p)))),
        t.dim,
    );

    vec![status, model, kv, requests, tokens, latency]
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

/// Model names are long; the full name stays in the head line.
fn short_model(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_string()
}

/// One dim line of lifetime totals and server-window rates: what this server
/// has done since it started, plus the readouts that have no card of their
/// own (cache hit rate, speculative decoding).
fn draw_lifetime(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let agg = e.aggregate();
    let mut spans: Vec<Span> = Vec::new();

    let served = series_sum(e, |s| s.requests_total());
    if served.is_some() {
        spans.push(Span::styled(" served ", t.dim));
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
    if !spans.is_empty() {
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn draw_head(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let now = Instant::now();
    let freshness = e.freshness(now, app.refresh_interval);
    let (badge, badge_style) = freshness_badge(app, freshness, e.healthy);

    let mut line1 = vec![
        Span::styled(format!(" {} ", e.name), t.heading),
        Span::styled(badge, badge_style),
        Span::styled(format!("  {}", e.display_url), t.dim),
        Span::styled("   SGLang ", t.dim),
        Span::styled(
            e.server_version
                .clone()
                .unwrap_or_else(|| format::NA.into()),
            t.secondary,
        ),
    ];
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
    if e.restart_seen_at
        .is_some_and(|at| now.saturating_duration_since(at).as_secs() < 30)
    {
        line1.push(Span::styled("  RESTARTED", t.crit));
    }
    // Which panel mode is showing; 't' cycles it.
    line1.push(Span::styled(
        format!("   [{}]", app.panel_mode.label()),
        t.dim,
    ));

    // Second line: only things worth flagging plus the served models.
    let mut line2: Vec<Span> = Vec::new();
    if e.parse_issue_count > 0 {
        line2.push(Span::styled(
            format!("   parse-issues {}", e.parse_issue_count),
            t.warn,
        ));
    }
    if !e.served_models.is_empty() {
        line2.push(Span::styled("   models ", t.dim));
        line2.push(Span::styled(
            format::truncate(
                &e.served_models
                    .iter()
                    .map(|m| m.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                40,
            ),
            t.secondary,
        ));
    }
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
            format!("  ✗{consecutive} {}", format::truncate(shown, 60)),
            t.crit,
        ));
    } else if loads_only {
        line2.push(Span::styled(
            "   gauges via /v1/loads — relaunch with --enable-metrics for rates & latencies",
            t.dim,
        ));
    }

    frame.render_widget(
        Paragraph::new(vec![Line::from(line1), Line::from(line2)])
            .block(Block::new().borders(Borders::BOTTOM).border_style(t.dim)),
        area,
    );
}

fn draw_tables(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)]).areas(area);
    draw_activity(frame, app, e, left);
    draw_latency(frame, app, e, right);
}

/// Left column: activity and cache metrics, one section per series.
fn draw_activity(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let mut lines: Vec<Line> = Vec::new();

    let Some(curated) = &e.curated else {
        lines.push(Line::from(Span::styled(" no data yet", t.na)));
        frame.render_widget(Paragraph::new(lines), area);
        return;
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
    frame.render_widget(Paragraph::new(lines), area);
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

/// Bottom charts of the tables view ('t'): generation trend + queue depth.
fn draw_trends(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(area);
    charts::draw_single_chart(
        frame,
        app,
        left,
        e,
        &charts::ChartSpec {
            title: "generation tokens/s",
            headline: None,
            series: &[charts::SeriesSpec {
                id: crate::state::series_id::GENERATION_TPS,
                label: "out",
            }],
            kind: charts::ValueKind::Count,
        },
    );
    charts::draw_single_chart(
        frame,
        app,
        right,
        e,
        &charts::ChartSpec {
            title: "running / waiting",
            headline: None,
            series: &[
                charts::SeriesSpec {
                    id: crate::state::series_id::RUNNING,
                    label: "run",
                },
                charts::SeriesSpec {
                    id: crate::state::series_id::WAITING,
                    label: "wait",
                },
            ],
            kind: charts::ValueKind::Count,
        },
    );
}

/// Default view, left half: prefill (prompt) and generation token rates as
/// stacked line charts, each with the live rate in its header. Headlines
/// come from the same aggregate the pulse strip uses, so they always agree.
fn draw_rate_charts(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let agg = e.aggregate();
    let rate = |v: Option<f64>| v.map(|v| format!("{} tokens/s", format::count(Some(v))));
    if area.height >= 12 {
        let [top, bottom] = Layout::vertical([Constraint::Percentage(50); 2]).areas(area);
        charts::draw_single_chart(
            frame,
            app,
            top,
            e,
            &charts::ChartSpec {
                title: "prefill tokens",
                headline: rate(agg.prompt_tps),
                series: &[charts::SeriesSpec {
                    id: crate::state::series_id::PROMPT_TPS,
                    label: "",
                }],
                kind: charts::ValueKind::Count,
            },
        );
        charts::draw_single_chart(
            frame,
            app,
            bottom,
            e,
            &charts::ChartSpec {
                title: "generation tokens",
                headline: rate(agg.generation_tps),
                series: &[charts::SeriesSpec {
                    id: crate::state::series_id::GENERATION_TPS,
                    label: "",
                }],
                kind: charts::ValueKind::Count,
            },
        );
    } else {
        // Too short for two readable charts: one combined chart.
        charts::draw_single_chart(
            frame,
            app,
            area,
            e,
            &charts::ChartSpec {
                title: "tokens/s",
                headline: None,
                series: &[
                    charts::SeriesSpec {
                        id: crate::state::series_id::PROMPT_TPS,
                        label: "in",
                    },
                    charts::SeriesSpec {
                        id: crate::state::series_id::GENERATION_TPS,
                        label: "out",
                    },
                ],
                kind: charts::ValueKind::Count,
            },
        );
    }
}

/// Default view, right half: recent requests from the log tailer.
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

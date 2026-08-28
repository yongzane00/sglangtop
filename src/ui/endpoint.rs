//! Tabs 2…N: one endpoint in depth.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::metrics::normalize::{CuratedSeries, SeriesKey, hist};
use crate::state::{DerivedSeries, EndpointState};
use crate::ui::{format, freshness_badge};

pub fn draw(frame: &mut Frame, app: &App, index: usize, area: Rect) {
    let Some(e) = app.endpoints.get(index) else {
        return;
    };
    let [head, pulse, body] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(4),
        Constraint::Min(0),
    ])
    .areas(area);
    draw_head(frame, app, e, head);
    draw_pulse(frame, app, index, e, pulse);

    if app.endpoint_tables {
        // 't' view: the ACTIVITY/LATENCY tables, charts below when roomy.
        if body.height >= 14 {
            let [tables, charts] =
                Layout::vertical([Constraint::Percentage(62), Constraint::Percentage(38)])
                    .areas(body);
            draw_tables(frame, app, e, tables);
            draw_trends(frame, app, e, charts);
        } else {
            draw_tables(frame, app, e, body);
        }
    } else {
        // Default view: token-rate charts left, live requests pane right.
        let [left, right] = Layout::horizontal([Constraint::Percentage(50); 2]).areas(body);
        draw_rate_charts(frame, app, e, left);
        draw_requests(frame, app, index, e, right);
    }
}

/// The "is it alive and how fast" strip: an animated GENERATING indicator,
/// the two token rates, running (as an `n/max` bar when the config declares
/// the server's `--max-num-seqs`), waiting, lifetime completions served, and
/// a full-width KV-cache bar that visibly grows while a conversation is
/// being generated.
fn draw_pulse(frame: &mut Frame, app: &App, index: usize, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let ascii = t.mode == crate::ui::theme::ColorMode::Mono;
    let agg = e.aggregate();
    let generating = agg.running.unwrap_or(0.0) > 0.0;

    // Status with a spinner while requests are running. The UI redraws at
    // least every 500 ms tick, so the animation runs at ~2 fps — enough to
    // read as "alive". (Display-only wall-clock use.)
    let mut line1: Vec<Span> = Vec::new();
    if generating {
        let frames: &[&str] = if ascii {
            &["|", "/", "-", "\\"]
        } else {
            &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
        };
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let glyph = frames[(ms / 250) as usize % frames.len()];
        line1.push(Span::styled(format!(" {glyph} GENERATING"), t.value));
    } else {
        line1.push(Span::styled(" · IDLE      ", t.dim));
    }
    line1.push(Span::styled("   generation ", t.dim));
    // `~` marks the server's own windowed estimate (loads-only fallback),
    // as opposed to our counter-derived rate.
    let gen_text = if agg.generation_tps_is_estimate {
        format!("~{} tokens/s", format::count(agg.generation_tps))
    } else {
        format!("{} tokens/s", format::count(agg.generation_tps))
    };
    line1.push(Span::styled(gen_text, t.value));
    if let Some(pk) = e.peak_generation_tps {
        line1.push(Span::styled(
            format!(" (pk {})", format::count(Some(pk))),
            t.dim,
        ));
    }
    line1.push(Span::styled("   prompt ", t.dim));
    line1.push(Span::styled(
        format!("{} tokens/s", format::count(agg.prompt_tps)),
        t.value,
    ));
    if let Some(pk) = e.peak_prompt_tps {
        line1.push(Span::styled(
            format!(" (pk {})", format::count(Some(pk))),
            t.dim,
        ));
    }
    // Second line: scheduler occupancy plus the lifetime served count —
    // line 1 already fills a 120-column terminal, so these get their own row.
    let mut line1b: Vec<Span> = vec![Span::styled("   running ", t.dim)];
    // Explicit config wins; otherwise the server's own --max-running-requests
    // reported via /server_info makes the n/max bar automatic.
    let configured = app.config.endpoints.get(index).and_then(|c| c.max_running);
    let max_running = e.effective_max_running(configured).filter(|m| *m > 0);
    match (agg.running, max_running) {
        (Some(run), Some(max)) => {
            let frac = run / max as f64;
            line1b.push(Span::styled(
                format!(
                    "{}/{} {}",
                    format::count(Some(run)),
                    max,
                    format::bar(frac, 8, ascii)
                ),
                t.by_level(frac, 0.75, 0.95),
            ));
        }
        (run, _) => line1b.push(Span::styled(format::count(run), t.value)),
    }
    line1b.push(Span::styled("   waiting ", t.dim));
    let wait_style = if agg.waiting.unwrap_or(0.0) > 0.0 {
        t.warn
    } else {
        t.value
    };
    line1b.push(Span::styled(format::count(agg.waiting), wait_style));

    // Lifetime completions since the server started (counter: resets with a
    // server restart, which is the honest reading of "served").
    let served = e.curated.as_ref().and_then(|c| {
        c.series
            .values()
            .filter_map(|s| s.requests_total())
            .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v))
    });
    if served.is_some() {
        line1b.push(Span::styled("   served ", t.dim));
        line1b.push(Span::styled(format::count(served), t.value));
    }

    // Lifetime token odometer (same restart-reset semantics as `served`):
    // "in" = prompt tokens consumed, "out" = tokens generated.
    let life_sum = |get: fn(&CuratedSeries) -> Option<f64>| {
        e.curated.as_ref().and_then(|c| {
            c.series
                .values()
                .filter_map(get)
                .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v))
        })
    };
    let prompt_life = life_sum(|s| s.prompt_tokens_total());
    let gen_life = life_sum(|s| s.generation_tokens_total());
    if prompt_life.is_some() || gen_life.is_some() {
        line1b.push(Span::styled("   tokens ", t.dim));
        line1b.push(Span::styled(
            format!(
                "{} in / {} out",
                format::count(prompt_life),
                format::count(gen_life)
            ),
            t.value,
        ));
    }

    // Full-width KV bar with absolute tokens when the server exposes its
    // capacity. This is the aggregate cache: with one conversation running
    // it is effectively that conversation's KV growing token by token.
    // The trailing cells are built first so the bar can take exactly the
    // space they leave (a fixed reserve overflowed once cache/spec joined
    // the line — Paragraph clips silently).
    let mut trailing: Vec<Span> = Vec::new();
    if let Some(kv) = agg.kv_usage {
        let frac = kv.value();
        trailing.push(Span::styled(
            format!(" {}", format::percent(Some(frac))),
            t.by_level(frac, 0.75, 0.9),
        ));
        if !kv.is_weighted() {
            trailing.push(Span::styled(" (unweighted)", t.warn));
        }
        // Absolute used/capacity, when every usage-reporting series also
        // exposes it. SGLang exports used tokens directly
        // (num_used_tokens); usage × capacity is only a fallback.
        if let Some(curated) = &e.curated {
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
            let usage_series = curated
                .series
                .values()
                .filter(|s| s.kv_cache_usage.is_some())
                .count();
            if !pairs.is_empty() && pairs.len() == usage_series {
                let total: f64 = pairs.iter().map(|(_, c)| c).sum();
                let used: f64 = pairs.iter().map(|(u, _)| u).sum();
                trailing.push(Span::styled(
                    format!(
                        "  {} / {} tok",
                        format::count(Some(used)),
                        format::count(Some(total))
                    ),
                    t.secondary,
                ));
            }
        }
    }
    // Cache hit rate: the server's own rolling window, never our invention.
    if let Some(rate) = agg.cache_hit_rate {
        trailing.push(Span::styled("   cache ", t.dim));
        trailing.push(Span::styled(
            format!("{} (srv win)", format::percent(Some(rate))),
            t.secondary,
        ));
    }
    // Speculative decoding, when exactly one series reports it (means are
    // not meaningful across series and per-series rows exist in the tables).
    if let Some(curated) = &e.curated {
        let specs: Vec<_> = curated
            .series
            .values()
            .filter(|s| s.spec_accept_rate.is_some() || s.spec_accept_length.is_some())
            .collect();
        if let [s] = specs.as_slice() {
            trailing.push(Span::styled("   spec ", t.dim));
            let mut parts: Vec<String> = Vec::new();
            if let Some(r) = s.spec_accept_rate {
                parts.push(format!("accept {}", format::percent(Some(r))));
            }
            if let Some(l) = s.spec_accept_length {
                parts.push(format!("{l:.1} tok/step"));
            }
            trailing.push(Span::styled(parts.join(", "), t.secondary));
        }
    }

    let mut line2: Vec<Span> = vec![Span::styled(" KV ", t.dim)];
    match agg.kv_usage {
        Some(kv) => {
            let frac = kv.value();
            let trailing_width: usize = trailing.iter().map(|s| s.content.chars().count()).sum();
            let bar_width = (area.width as usize)
                .saturating_sub(4 + trailing_width + 1)
                .clamp(10, 80);
            line2.push(Span::styled(
                format::bar(frac, bar_width, ascii),
                t.by_level(frac, 0.75, 0.9),
            ));
        }
        None => line2.push(Span::styled(format::NA, t.na)),
    }
    line2.extend(trailing);

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(line1),
            Line::from(line1b),
            Line::from(line2),
        ])
        .block(Block::new().borders(Borders::BOTTOM).border_style(t.dim)),
        area,
    );
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
            format::truncate(&e.served_models.join(", "), 40),
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
    super::charts::draw_single_chart(
        frame,
        app,
        left,
        "generation tokens/s",
        None,
        &[crate::state::series_id::GENERATION_TPS],
        Some(e),
    );
    super::charts::draw_single_chart(
        frame,
        app,
        right,
        "running / waiting",
        None,
        &[
            crate::state::series_id::RUNNING,
            crate::state::series_id::WAITING,
        ],
        Some(e),
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
        super::charts::draw_single_chart(
            frame,
            app,
            top,
            "prefill tokens",
            rate(agg.prompt_tps),
            &[crate::state::series_id::PROMPT_TPS],
            Some(e),
        );
        super::charts::draw_single_chart(
            frame,
            app,
            bottom,
            "generation tokens",
            rate(agg.generation_tps),
            &[crate::state::series_id::GENERATION_TPS],
            Some(e),
        );
    } else {
        // Too short for two readable charts: one combined chart.
        super::charts::draw_single_chart(
            frame,
            app,
            area,
            "prefill + generation tokens",
            rate(agg.generation_tps),
            &[
                crate::state::series_id::PROMPT_TPS,
                crate::state::series_id::GENERATION_TPS,
            ],
            Some(e),
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
        ["age", "prompt", "req id", "max_tok"]
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
            let prompt = match &entry.prompt_preview {
                Some(p) => Span::styled(p.clone(), row_style),
                None => Span::styled(format::NA, t.na),
            };
            Row::new(vec![
                Cell::from(Span::styled(age, t.dim)),
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

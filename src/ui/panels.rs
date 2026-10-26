//! The endpoint overview's lower band: key percentiles, server/model info,
//! and the observed-events feed.
//!
//! Honesty rules these panels exist to keep:
//! - `mem_fraction_static` is the **configured** target the operator asked
//!   for, never a measurement — the measured figures are the `*_memory_gb`
//!   gauges next to it;
//! - `startup free` GPU memory was measured at startup, not now;
//! - the CPU seconds are the **frontend** tokenizer/detokenizer processes,
//!   not the scheduler and not the GPU worker.

use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use std::time::Instant;

use crate::app::App;
use crate::metrics::normalize::hist;
use crate::state::EndpointState;
use crate::ui::format;

fn panel<'a>(app: &App, title: &'a str) -> Block<'a> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(app.theme.dim)
        .title(Span::styled(format!(" {title} "), app.theme.heading))
}

/// The headline latency distributions, aggregated as "worst across series"
/// so a PD-disaggregated or DP-sharded endpoint cannot hide its slowest
/// shard behind a mean.
pub fn draw_percentiles(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let window = app.config.percentile_window.as_secs();
    let title = format!("percentiles (~{window}s est)");
    let block = panel(app, &title);

    let worst =
        |id: &'static str, pick: fn(&crate::metrics::histogram::WindowEstimate) -> Option<f64>| {
            e.derived
                .values()
                .filter_map(|d| d.estimates.get(id))
                .filter_map(pick)
                .fold(None, |acc: Option<f64>, v| {
                    Some(acc.map_or(v, |cur: f64| cur.max(v)))
                })
        };
    let cell = |v: Option<f64>, seconds: bool| {
        let text = if seconds {
            format::seconds(v)
        } else {
            format::count(v)
        };
        Cell::from(Span::styled(text, if v.is_some() { t.value } else { t.na }))
    };
    let row = |label: &'static str, id: &'static str, seconds: bool| {
        Row::new(vec![
            Cell::from(Span::styled(label, t.dim)),
            cell(worst(id, |e| e.p50), seconds),
            cell(worst(id, |e| e.p95), seconds),
            cell(worst(id, |e| e.p99), seconds),
        ])
    };

    let rows = vec![
        row("TTFT", hist::TTFT, true),
        row("e2e", hist::E2E_LATENCY, true),
        row("inter-token", hist::INTER_TOKEN_LATENCY, true),
        row("queue", hist::QUEUE_TIME, true),
        row("prompt tok/req", hist::PROMPT_TOKENS_PER_REQ, false),
        row("gen tok/req", hist::GENERATION_TOKENS_PER_REQ, false),
    ];
    let header = Row::new(
        ["", "p50", "p95", "p99"]
            .into_iter()
            .map(|h| Cell::from(Span::styled(h, t.heading))),
    );
    let widths = [
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
    ];
    frame.render_widget(
        Table::new(rows, widths)
            .header(header)
            .column_spacing(1)
            .block(block),
        area,
    );
}

/// Server and model facts: what this endpoint is, how it was configured, and
/// where its GPU memory went. SGLang reports memory in GB directly, so these
/// are measurements rather than inferences.
pub fn draw_info(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let block = panel(app, "server & model");
    let info = e.curated.as_ref().map(|c| &c.info);

    let mut lines: Vec<Line> = Vec::new();
    let mut row = |label: &str, value: String, style| {
        lines.push(Line::from(vec![
            Span::styled(format!(" {label:<16}"), t.dim),
            Span::styled(value, style),
        ]));
    };
    let series_sum = |get: fn(&crate::metrics::normalize::CuratedSeries) -> Option<f64>| {
        e.curated.as_ref().and_then(|c| {
            c.series
                .values()
                .filter_map(get)
                .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v))
        })
    };

    // Identity.
    if let Some(model) = e.served_models.first() {
        row("model", format::truncate(&model.id, 28), t.value);
        if let Some(root) = &model.root {
            row("path", format::truncate(root, 28), t.secondary);
        }
    }
    let ctx = e
        .served_models
        .iter()
        .find_map(|m| m.max_model_len.map(|c| c as f64))
        .or_else(|| series_sum(|s| s.context_len));
    row("context", format::count(ctx), t.secondary);
    if let Some(v) = &e.server_version {
        row("SGLang", v.clone(), t.secondary);
    }

    // Topology and features, from /server_info.
    if let Some(si) = &e.server_info {
        let par: Vec<String> = [("tp", si.tp_size), ("dp", si.dp_size)]
            .into_iter()
            .filter_map(|(k, v)| v.map(|v| format!("{k}{v}")))
            .collect();
        if !par.is_empty() {
            row("parallelism", par.join(" · "), t.secondary);
        }
        if let Some(role) = si.pd_role() {
            row("PD role", role.to_string(), t.warn);
        }
        if let Some(algo) = &si.speculative_algorithm {
            row("speculative", algo.clone(), t.secondary);
        }
    }

    // KV pool, in tokens.
    if let Some(cap) = series_sum(|s| s.kv_cache_size_tokens) {
        row(
            "kv capacity",
            format!("{} tok", format::count(Some(cap))),
            t.value,
        );
    }

    // GPU memory — measured, in GB.
    let mem = [
        ("weights", series_sum(|s| s.weight_memory_gb)),
        ("kv cache", series_sum(|s| s.kv_cache_memory_gb)),
        ("cuda graphs", series_sum(|s| s.graph_memory_gb)),
    ];
    for (label, value) in mem {
        if let Some(gb) = value {
            row(label, format!("{gb:.2} GB"), t.secondary);
        }
    }
    if let Some(free) = series_sum(|s| s.startup_free_memory_gb) {
        // Measured once at startup; not live headroom.
        row("free at startup", format!("{free:.2} GB"), t.dim);
    }

    // Frontend counters.
    if let Some(info) = info {
        if !info.http_by_status.is_empty() {
            let text = info
                .http_by_status
                .iter()
                .map(|(class, n)| format!("{class} {}", format::count(Some(*n))))
                .collect::<Vec<_>>()
                .join("  ");
            let style = if info.http_by_status.keys().any(|k| k.starts_with('5')) {
                t.warn
            } else {
                t.secondary
            };
            row("http", text, style);
        }
        if let Some(active) = info.http_active {
            row("http in flight", format::count(Some(active)), t.secondary);
        }
        if let Some(cpu) = info.cpu_total() {
            row(
                "frontend cpu",
                format!("{} total", format::seconds(Some(cpu))),
                t.secondary,
            );
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(" no metadata yet", t.na)));
    }
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// What sglangtop has observed about this endpoint — our own notes, not the
/// server's log.
pub fn draw_events(frame: &mut Frame, app: &App, e: &EndpointState, area: Rect) {
    let t = &app.theme;
    let block = panel(app, "events");
    let now = Instant::now();

    if e.events.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(" nothing noteworthy yet", t.na))).block(block),
            area,
        );
        return;
    }

    let visible = area.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = e
        .events
        .iter_newest_first()
        .take(visible.max(1))
        .map(|ev| {
            let age = format::brief_duration(now.saturating_duration_since(ev.at));
            Line::from(vec![
                Span::styled(format!(" {age:>6} "), t.dim),
                Span::styled(
                    format!("{:<11}", ev.kind.label()),
                    if ev.kind.is_bad() { t.crit } else { t.value },
                ),
                Span::styled(ev.detail.clone(), t.text),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

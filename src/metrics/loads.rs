//! `/v1/loads` fallback source: SGLang serves per-DP-rank load snapshots as
//! JSON on every build, with or without `--enable-metrics`. When `/metrics`
//! 404s (metrics disabled), these gauges keep the dashboard honest instead
//! of blank: everything counter-derived (token rates, completions/s,
//! percentiles, daily usage) is genuinely absent and stays `--`.
//!
//! Snapshots carry no model name; the caller passes one resolved from
//! `/v1/models` or `/server_info`.

use serde::Deserialize;

use super::normalize::{CuratedScrape, CuratedSeries, SeriesKey};

/// Response of `GET /v1/loads`. Defensive: every field optional, unknown
/// fields ignored, so a snapshot-shape change never kills the fallback.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LoadsResponse {
    pub version: Option<String>,
    pub loads: Vec<LoadSnapshot>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LoadSnapshot {
    pub dp_rank: Option<i64>,
    pub num_running_reqs: Option<f64>,
    pub num_waiting_reqs: Option<f64>,
    pub num_used_tokens: Option<f64>,
    pub max_total_num_tokens: Option<f64>,
    pub max_running_requests: Option<f64>,
    pub token_usage: Option<f64>,
    pub gen_throughput: Option<f64>,
    pub cache_hit_rate: Option<f64>,
    pub speculative: Option<SpeculativeLoad>,
    pub disaggregation: Option<DisaggregationLoad>,
    pub queues: Option<QueuesLoad>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SpeculativeLoad {
    pub accept_length: Option<f64>,
    pub accept_rate: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct DisaggregationLoad {
    /// `"prefill"` / `"decode"`, or the literal string `"null"` when unified.
    pub mode: Option<String>,
    pub prefill_bootstrap_queue_reqs: Option<f64>,
    pub prefill_inflight_queue_reqs: Option<f64>,
    pub decode_prealloc_queue_reqs: Option<f64>,
    pub decode_transfer_queue_reqs: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QueuesLoad {
    pub grammar: Option<f64>,
    pub paused: Option<f64>,
    pub retracted: Option<f64>,
}

/// Capability marker: this scrape came from `/v1/loads`, not `/metrics`.
pub const LOADS_ONLY: &str = "loads_only";

fn finite(v: Option<f64>) -> Option<f64> {
    v.filter(|v| v.is_finite())
}

/// Map load snapshots onto the same curated model the Prometheus path
/// produces — one series per DP rank — so state, history, recorder, and UI
/// need no special cases. Counters and histograms stay absent by
/// construction; `gen_throughput` flows through the flagged server-estimate
/// fallback in the state layer.
pub fn curate_loads(loads: &LoadsResponse, model: &str) -> CuratedScrape {
    let mut out = CuratedScrape::default();
    out.capabilities.push(LOADS_ONLY);
    let single = loads.loads.len() == 1;
    for (idx, snap) in loads.loads.iter().enumerate() {
        let engine_type = match snap.disaggregation.as_ref().and_then(|d| d.mode.as_deref()) {
            Some("prefill") => Some("prefill".to_string()),
            Some("decode") => Some("decode".to_string()),
            _ => None,
        };
        let key = SeriesKey {
            model: model.to_string(),
            engine_type: engine_type.clone(),
            // A lone snapshot matches the /metrics identity (no dp_rank
            // label); only real DP fan-out keeps the rank dimension. The
            // array index backstops a response whose snapshots omit
            // `dp_rank`, so distinct ranks can never collide onto one key
            // and silently overwrite each other's gauges.
            dp_rank: if single {
                None
            } else {
                Some(
                    snap.dp_rank
                        .map_or_else(|| idx.to_string(), |r| r.to_string()),
                )
            },
        };
        let series: &mut CuratedSeries = out.series.entry(key).or_default();
        series.running = finite(snap.num_running_reqs);
        series.waiting = finite(snap.num_waiting_reqs);
        series.kv_cache_usage = finite(snap.token_usage);
        series.kv_used_tokens = finite(snap.num_used_tokens);
        series.kv_cache_size_tokens = finite(snap.max_total_num_tokens);
        series.cache_hit_rate_recent = finite(snap.cache_hit_rate);
        series.gen_throughput_server = finite(snap.gen_throughput);
        if let Some(spec) = &snap.speculative {
            // Same value-gating as the Prometheus path: 0.0 means "off".
            series.spec_accept_length = finite(spec.accept_length).filter(|v| *v > 0.0);
            series.spec_accept_rate = finite(spec.accept_rate).filter(|v| *v > 0.0);
        }
        if let Some(q) = &snap.queues {
            for (name, value) in [
                ("grammar", q.grammar),
                ("paused", q.paused),
                ("retracted", q.retracted),
            ] {
                if let Some(v) = finite(value) {
                    series.waiting_extras.insert(name, v);
                }
            }
        }
        if engine_type.is_some()
            && let Some(d) = &snap.disaggregation
        {
            series.pd_bootstrap_queue = finite(d.prefill_bootstrap_queue_reqs);
            series.pd_inflight_queue = finite(d.prefill_inflight_queue_reqs);
            series.pd_prealloc_queue = finite(d.decode_prealloc_queue_reqs);
            series.pd_transfer_queue = finite(d.decode_transfer_queue_reqs);
        }
    }
    for cap in [
        "running",
        "waiting",
        "kv_usage",
        "kv_used",
        "kv_capacity",
        "cache_hit_rate",
        "gen_throughput",
    ] {
        if out.series.values().any(|s| match cap {
            "running" => s.running.is_some(),
            "waiting" => s.waiting.is_some(),
            "kv_usage" => s.kv_cache_usage.is_some(),
            "kv_used" => s.kv_used_tokens.is_some(),
            "kv_capacity" => s.kv_cache_size_tokens.is_some(),
            "cache_hit_rate" => s.cache_hit_rate_recent.is_some(),
            "gen_throughput" => s.gen_throughput_server.is_some(),
            _ => false,
        }) {
            out.capabilities.push(cap);
        }
    }
    if out.series.values().any(|s| s.spec_accept_length.is_some()) {
        out.capabilities.push("spec");
    }
    if out.series.keys().any(|k| k.engine_type.is_some()) {
        out.capabilities.push("pd_queues");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str = include_str!("../../tests/fixtures/sglang_v1_loads.json");

    #[test]
    fn live_loads_capture_deserializes_and_curates() {
        let loads: LoadsResponse = serde_json::from_str(LIVE).expect("real capture parses");
        assert_eq!(loads.loads.len(), 1);
        let curated = curate_loads(&loads, "Qwen/Qwen3.8-27B-FP8");
        assert!(curated.has(LOADS_ONLY));
        let key = SeriesKey {
            model: "Qwen/Qwen3.8-27B-FP8".into(),
            engine_type: None,
            dp_rank: None, // single snapshot: no dp dimension
        };
        let s = &curated.series[&key];
        assert_eq!(s.running, Some(1.0));
        assert_eq!(s.waiting, Some(0.0));
        assert_eq!(s.kv_cache_usage, Some(0.1828));
        assert_eq!(s.kv_used_tokens, Some(23_952.0));
        assert_eq!(s.kv_cache_size_tokens, Some(131_016.0));
        assert!(s.spec_accept_length.unwrap() > 2.8);
        assert!(s.spec_accept_rate.unwrap() > 0.55);
        // Nothing counter-derived exists — honest absence.
        assert!(s.realtime_tokens_by_mode.is_empty());
        assert!(s.prompt_tokens_by_stream.is_empty());
        assert!(s.histograms.is_empty());
        assert_eq!(s.aborted, None);
    }

    #[test]
    fn dp_fanout_keeps_ranks_and_unified_mode_drops_engine_type() {
        let json = r#"{"loads":[
            {"dp_rank":0,"num_running_reqs":2,"disaggregation":{"mode":"null"}},
            {"dp_rank":1,"num_running_reqs":3,"disaggregation":{"mode":"null"}}
        ]}"#;
        let loads: LoadsResponse = serde_json::from_str(json).unwrap();
        let curated = curate_loads(&loads, "m");
        assert_eq!(curated.series.len(), 2);
        let k1 = SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: Some("1".into()),
        };
        assert_eq!(curated.series[&k1].running, Some(3.0));
    }

    #[test]
    fn pd_mode_lands_queues_on_role_series() {
        let json = r#"{"loads":[
            {"dp_rank":0,"num_running_reqs":4,
             "disaggregation":{"mode":"decode","decode_prealloc_queue_reqs":5,
                               "decode_transfer_queue_reqs":6}}
        ]}"#;
        let loads: LoadsResponse = serde_json::from_str(json).unwrap();
        let curated = curate_loads(&loads, "m");
        let key = SeriesKey {
            model: "m".into(),
            engine_type: Some("decode".into()),
            dp_rank: None,
        };
        let s = &curated.series[&key];
        assert_eq!(s.pd_prealloc_queue, Some(5.0));
        assert_eq!(s.pd_transfer_queue, Some(6.0));
        assert!(curated.has("pd_queues"));
    }

    #[test]
    fn snapshots_without_dp_rank_do_not_collide_onto_one_key() {
        // The deserializer is deliberately permissive, so a response whose
        // snapshots omit dp_rank must still keep one series per snapshot —
        // otherwise the last one silently overwrites the others' gauges.
        let json = r#"{"loads":[
            {"num_running_reqs":2,"num_used_tokens":50000},
            {"num_running_reqs":3}
        ]}"#;
        let loads: LoadsResponse = serde_json::from_str(json).unwrap();
        let curated = curate_loads(&loads, "m");
        assert_eq!(curated.series.len(), 2);
        let by_rank = |d: &str| SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: Some(d.into()),
        };
        assert_eq!(curated.series[&by_rank("0")].running, Some(2.0));
        assert_eq!(curated.series[&by_rank("0")].kv_used_tokens, Some(50000.0));
        assert_eq!(curated.series[&by_rank("1")].running, Some(3.0));
    }

    #[test]
    fn spec_gauges_value_gate_in_loads_too() {
        let json = r#"{"loads":[
            {"dp_rank":0,"speculative":{"accept_length":0.0,"accept_rate":0.0}}
        ]}"#;
        let loads: LoadsResponse = serde_json::from_str(json).unwrap();
        let curated = curate_loads(&loads, "m");
        let s = curated.series.values().next().unwrap();
        assert_eq!(s.spec_accept_length, None);
        assert_eq!(s.spec_accept_rate, None);
        assert!(!curated.has("spec"));
    }
}

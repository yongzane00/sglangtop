//! Capability-based extraction of curated SGLang metrics.
//!
//! This is the only place that knows concrete `sglang:*` metric name strings.
//! Each curated quantity has a canonical id, so an upstream rename (e.g.
//! `sglang:time_per_output_token_seconds` → `sglang:inter_token_latency_seconds`)
//! is a one-line change here. Anything not curated stays visible in the Raw
//! Metrics view; anything missing surfaces as `None` (rendered `--`), never 0.
//!
//! Label model (verified against a live SGLang 0.5.19 server):
//! - Scheduler metrics carry `model_name`, `engine_type` (`prefill` |
//!   `decode` | `unified`), `tp_rank`, `pp_rank`, `moe_ep_rank`, and
//!   `dp_rank` only when DP attention is on. Some gauges additionally carry a
//!   `pid` label (prometheus multiprocess children).
//! - Tokenizer (frontend) metrics carry only `model_name` and `engine_type`,
//!   plus residual fan-out labels: `is_streaming` on token/request counters
//!   and the TTFT/e2e histograms, `cache_source` on `cached_tokens_total`.
//!
//! Series identity is (model_name, engine_type≠unified, dp_rank). The
//! replica dimensions `tp_rank`/`pp_rank`/`moe_ep_rank` are *projected*: only
//! the numerically smallest rank tuple per series is curated, because under
//! `--enable-metrics-for-all-schedulers` every rank exports a near-identical
//! copy and summing them would multiply every value by the TP size.
//! Residual labels are merged instead: counters and histogram buckets sum
//! (additive by construction); intensive gauges are taken only when all
//! samples agree; extensive gauges (queue depths fanned out by `pid`) sum.

use std::collections::BTreeMap;

use super::model::{MetricFamily, MetricType, Sample, ScrapeText};

/// Identity of one exported series.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SeriesKey {
    pub model: String,
    /// `engine_type` label when PD-disaggregated (`prefill`/`decode`);
    /// `None` for the default `unified` mode (normalized at construction).
    pub engine_type: Option<String>,
    /// `dp_rank` label when DP attention is on.
    pub dp_rank: Option<String>,
}

impl SeriesKey {
    pub fn display(&self) -> String {
        match (&self.engine_type, &self.dp_rank) {
            (None, None) => self.model.clone(),
            (Some(e), None) => format!("{} [{}]", self.model, e),
            (None, Some(d)) => format!("{} [dp{}]", self.model, d),
            (Some(e), Some(d)) => format!("{} [{} dp{}]", self.model, e, d),
        }
    }

    /// Compact serialization of the non-model dimensions, for the SQLite
    /// `engine` column and anywhere else a single optional string is stored.
    pub fn engine_str(&self) -> Option<String> {
        match (&self.engine_type, &self.dp_rank) {
            (None, None) => None,
            (Some(e), None) => Some(e.clone()),
            (None, Some(d)) => Some(format!("dp{d}")),
            (Some(e), Some(d)) => Some(format!("{e} dp{d}")),
        }
    }
}

/// Cumulative histogram data for one series, straight from a scrape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawHistogram {
    /// `(le, cumulative_count)`, unsorted here; sorted/validated downstream.
    pub buckets: Vec<(f64, f64)>,
    pub sum: f64,
    pub count: f64,
}

/// Canonical ids for curated histograms (UI/state key on these).
pub mod hist {
    pub const TTFT: &str = "ttft";
    pub const INTER_TOKEN_LATENCY: &str = "itl";
    pub const E2E_LATENCY: &str = "e2e";
    pub const QUEUE_TIME: &str = "queue";
    pub const PROMPT_TOKENS_PER_REQ: &str = "req_prompt_tokens";
    pub const GENERATION_TOKENS_PER_REQ: &str = "req_generation_tokens";
    pub const UNCACHED_PROMPT_TOKENS_PER_REQ: &str = "req_uncached_prompt_tokens";
    /// PD disaggregation: KV transfer latency (exported in ms, curated in s).
    pub const KV_TRANSFER_LATENCY: &str = "kv_transfer_latency";

    /// Per-stage request latency ids, one per allowed `stage` label value of
    /// `sglang:per_stage_req_latency_seconds`. Unknown stages stay raw-only.
    pub const STAGE_REQUEST_PROCESS: &str = "stage:request_process";
    pub const STAGE_PREFILL_FORWARD: &str = "stage:prefill_forward";
    pub const STAGE_CHUNKED_PREFILL: &str = "stage:chunked_prefill";
    pub const STAGE_PREFILL_BOOTSTRAP: &str = "stage:prefill_bootstrap";
    pub const STAGE_PREFILL_TRANSFER_KV: &str = "stage:prefill_transfer_kv_cache";
    pub const STAGE_DECODE_PREPARE: &str = "stage:decode_prepare";
    pub const STAGE_DECODE_BOOTSTRAP: &str = "stage:decode_bootstrap";
    pub const STAGE_DECODE_WAITING: &str = "stage:decode_waiting";
    pub const STAGE_DECODE_TRANSFERRED: &str = "stage:decode_transferred";

    /// `stage` label value → canonical id.
    pub const STAGES: &[(&str, &str)] = &[
        ("request_process", STAGE_REQUEST_PROCESS),
        ("prefill_forward", STAGE_PREFILL_FORWARD),
        ("chunked_prefill", STAGE_CHUNKED_PREFILL),
        ("prefill_bootstrap", STAGE_PREFILL_BOOTSTRAP),
        ("prefill_transfer_kv_cache", STAGE_PREFILL_TRANSFER_KV),
        ("decode_prepare", STAGE_DECODE_PREPARE),
        ("decode_bootstrap", STAGE_DECODE_BOOTSTRAP),
        ("decode_waiting", STAGE_DECODE_WAITING),
        ("decode_transferred", STAGE_DECODE_TRANSFERRED),
    ];
}

/// `(canonical id, exposition names newest-first)` for plain histograms.
/// (`per_stage_req_latency_seconds` and `kv_transfer_latency_ms` are routed
/// separately: one fans out over `stage`, the other needs a unit conversion.)
const HISTOGRAM_SPECS: &[(&str, &[&str])] = &[
    (hist::TTFT, &["sglang:time_to_first_token_seconds"]),
    (
        hist::INTER_TOKEN_LATENCY,
        &[
            "sglang:inter_token_latency_seconds",
            // Pre-0.4.9 spelling.
            "sglang:time_per_output_token_seconds",
        ],
    ),
    (hist::E2E_LATENCY, &["sglang:e2e_request_latency_seconds"]),
    (hist::QUEUE_TIME, &["sglang:queue_time_seconds"]),
    (
        hist::PROMPT_TOKENS_PER_REQ,
        &["sglang:prompt_tokens_histogram"],
    ),
    (
        hist::GENERATION_TOKENS_PER_REQ,
        &["sglang:generation_tokens_histogram"],
    ),
    (
        hist::UNCACHED_PROMPT_TOKENS_PER_REQ,
        &["sglang:uncached_prompt_tokens_histogram"],
    ),
];

const STAGE_HISTOGRAM: &str = "sglang:per_stage_req_latency_seconds";
const KV_TRANSFER_HISTOGRAM_MS: &str = "sglang:kv_transfer_latency_ms";

/// Bucket key for counter samples that lack their family's routing label
/// (see [`map_counter`]). Distinct from any real label value.
pub const UNROUTED: &str = "(unlabelled)";

/// Curated values for one series. Cumulative counters stay cumulative here;
/// rate computation happens in the state layer where monotonic scrape times
/// live.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CuratedSeries {
    // --- Scheduler gauges ---
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    /// Auxiliary queue depths (`grammar`, `paused`, `retracted` gauges),
    /// shown inline when non-zero.
    pub waiting_extras: BTreeMap<&'static str, f64>,
    /// `sglang:token_usage`: KV token-pool usage, 0.0..=1.0 fraction.
    pub kv_cache_usage: Option<f64>,
    /// Hybrid-model pool detail (`full`/`swa`/`mamba`), kept only when > 0.
    pub kv_usage_detail: BTreeMap<&'static str, f64>,
    /// `sglang:num_used_tokens`: used slots in the KV token pool.
    pub kv_used_tokens: Option<f64>,
    /// `sglang:max_total_num_tokens`: KV pool capacity in tokens. A directly
    /// labelled gauge — no join machinery needed (unlike vLLM).
    pub kv_cache_size_tokens: Option<f64>,
    /// `sglang:cache_hit_rate`: the SERVER's rolling-window prefix-cache hit
    /// rate (0..1). Label it "(server window)" — it is not lifetime.
    pub cache_hit_rate_recent: Option<f64>,
    /// `sglang:gen_throughput`: server-computed decode token/s over its own
    /// log window. Display fallback only; counter-derived rates are primary.
    pub gen_throughput_server: Option<f64>,
    /// Speculative decoding gauges; registered unconditionally upstream and
    /// 0.0 when off, so values <= 0 are curated as absent.
    pub spec_accept_length: Option<f64>,
    pub spec_accept_rate: Option<f64>,
    // PD-disaggregation queue gauges (kept only on prefill/decode series;
    // unified servers register them permanently at 0).
    pub pd_bootstrap_queue: Option<f64>,
    pub pd_inflight_queue: Option<f64>,
    pub pd_prealloc_queue: Option<f64>,
    pub pd_transfer_queue: Option<f64>,

    // --- Scheduler counters (cumulative) ---
    /// mode → cumulative tokens (`prefill_compute`, `prefill_cache`,
    /// `decode`). Incremented per forward batch — the live rate source.
    pub realtime_tokens_by_mode: BTreeMap<String, f64>,
    /// `sglang:num_retracted_requests_total`: decode-OOM requeues (the
    /// preemption analogue). Materializes on first retraction.
    pub retracted: Option<f64>,

    // --- Tokenizer counters (cumulative; children keyed by `is_streaming`).
    // Incremented at request FINISH (lump attribution): fine for lifetime
    // odometers and daily usage, wrong for live token rates — use
    // `realtime_tokens_by_mode` for those.
    pub prompt_tokens_by_stream: BTreeMap<String, f64>,
    pub generation_tokens_by_stream: BTreeMap<String, f64>,
    pub requests_by_stream: BTreeMap<String, f64>,
    /// `sglang:num_aborted_requests_total`; aborts are NOT included in
    /// `requests_by_stream`. Materializes on first abort.
    pub aborted: Option<f64>,
    /// cache_source (`device`/`host`/`storage`) → cumulative cached tokens.
    pub cached_tokens_by_source: BTreeMap<String, f64>,

    // --- Histograms (cumulative snapshots), keyed by canonical id ---
    pub histograms: BTreeMap<&'static str, RawHistogram>,
}

impl CuratedSeries {
    /// Lifetime prompt tokens across streaming modes.
    pub fn prompt_tokens_total(&self) -> Option<f64> {
        sum_map(&self.prompt_tokens_by_stream)
    }

    /// Lifetime generation tokens across streaming modes.
    pub fn generation_tokens_total(&self) -> Option<f64> {
        sum_map(&self.generation_tokens_by_stream)
    }

    /// Lifetime finished requests across streaming modes (excludes aborts).
    pub fn requests_total(&self) -> Option<f64> {
        sum_map(&self.requests_by_stream)
    }

    /// Lifetime cached prompt tokens across cache tiers.
    pub fn cached_tokens_total(&self) -> Option<f64> {
        sum_map(&self.cached_tokens_by_source)
    }
}

fn sum_map<K: Ord>(map: &BTreeMap<K, f64>) -> Option<f64> {
    if map.is_empty() {
        None
    } else {
        Some(map.values().sum())
    }
}

/// The curated view of one whole scrape.
#[derive(Debug, Clone, Default)]
pub struct CuratedScrape {
    pub series: BTreeMap<SeriesKey, CuratedSeries>,
    /// Canonical ids of curated metrics that were actually found — the
    /// endpoint's detected capabilities.
    pub capabilities: Vec<&'static str>,
}

impl CuratedScrape {
    pub fn has(&self, capability: &'static str) -> bool {
        self.capabilities.contains(&capability)
    }
}

fn series_key(labels: &super::model::LabelSet) -> Option<SeriesKey> {
    let model = labels.get("model_name")?.to_string();
    let engine_type = match labels.get("engine_type") {
        Some("unified") | None => None,
        Some(other) => Some(other.to_string()),
    };
    Some(SeriesKey {
        model,
        engine_type,
        dp_rank: labels.get("dp_rank").map(str::to_string),
    })
}

/// Replica-rank tuple for projection. Absent labels sort first, numeric
/// values sort numerically, non-numeric values sort after all numerics —
/// so "the" scheduler in a default deployment and rank 0 in a multi-rank
/// one are both selected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
struct RankTuple(Option<RankVal>, Option<RankVal>, Option<RankVal>);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum RankVal {
    Num(u64),
    Text(String),
}

fn rank_val(labels: &super::model::LabelSet, name: &str) -> Option<RankVal> {
    labels.get(name).map(|raw| match raw.parse::<u64>() {
        Ok(n) => RankVal::Num(n),
        Err(_) => RankVal::Text(raw.to_string()),
    })
}

fn rank_tuple(labels: &super::model::LabelSet) -> RankTuple {
    RankTuple(
        rank_val(labels, "tp_rank"),
        rank_val(labels, "pp_rank"),
        rank_val(labels, "moe_ep_rank"),
    )
}

/// Group a family's samples by series key, keeping only each key's minimum
/// rank tuple (see module docs). Samples without `model_name` are dropped.
fn rank_projected(family: &MetricFamily) -> BTreeMap<SeriesKey, Vec<&Sample>> {
    let mut min_rank: BTreeMap<SeriesKey, RankTuple> = BTreeMap::new();
    for sample in &family.samples {
        let Some(key) = series_key(&sample.labels) else {
            continue;
        };
        let rank = rank_tuple(&sample.labels);
        min_rank
            .entry(key)
            .and_modify(|m| {
                if rank < *m {
                    *m = rank.clone();
                }
            })
            .or_insert(rank);
    }
    let mut grouped: BTreeMap<SeriesKey, Vec<&Sample>> = BTreeMap::new();
    for sample in &family.samples {
        let Some(key) = series_key(&sample.labels) else {
            continue;
        };
        if min_rank.get(&key) == Some(&rank_tuple(&sample.labels)) {
            grouped.entry(key).or_default().push(sample);
        }
    }
    grouped
}

/// Extract curated metrics from a parsed scrape.
pub fn curate(scrape: &ScrapeText) -> CuratedScrape {
    let mut out = CuratedScrape::default();

    for family in &scrape.families {
        match family.name.as_str() {
            // --- Scheduler gauges (intensive: agree-or-absent) ---
            "sglang:num_running_reqs" => {
                intensive_gauge(family, &mut out, "running", |s, v| s.running = Some(v));
            }
            "sglang:num_queue_reqs" => {
                intensive_gauge(family, &mut out, "waiting", |s, v| s.waiting = Some(v));
            }
            "sglang:token_usage" => {
                intensive_gauge(family, &mut out, "kv_usage", |s, v| {
                    s.kv_cache_usage = Some(v)
                });
            }
            "sglang:num_used_tokens" => {
                intensive_gauge(family, &mut out, "kv_used", |s, v| {
                    s.kv_used_tokens = Some(v)
                });
            }
            "sglang:max_total_num_tokens" => {
                intensive_gauge(family, &mut out, "kv_capacity", |s, v| {
                    s.kv_cache_size_tokens = Some(v)
                });
            }
            "sglang:cache_hit_rate" => {
                intensive_gauge(family, &mut out, "cache_hit_rate", |s, v| {
                    s.cache_hit_rate_recent = Some(v)
                });
            }
            "sglang:gen_throughput" => {
                intensive_gauge(family, &mut out, "gen_throughput", |s, v| {
                    s.gen_throughput_server = Some(v)
                });
            }
            // Speculative decoding: registered unconditionally, 0.0 when
            // off — a value <= 0 is "feature absent", not a real zero.
            "sglang:spec_accept_length" => {
                intensive_gauge_gated(family, &mut out, "spec", |s, v| {
                    s.spec_accept_length = Some(v)
                });
            }
            "sglang:spec_accept_rate" => {
                intensive_gauge_gated(family, &mut out, "spec", |s, v| {
                    s.spec_accept_rate = Some(v)
                });
            }
            // Hybrid-model pool detail: same registered-at-zero situation.
            "sglang:full_token_usage" => {
                intensive_gauge_gated(family, &mut out, "kv_detail", |s, v| {
                    s.kv_usage_detail.insert("full", v);
                });
            }
            "sglang:swa_token_usage" => {
                intensive_gauge_gated(family, &mut out, "kv_detail", |s, v| {
                    s.kv_usage_detail.insert("swa", v);
                });
            }
            "sglang:mamba_usage" => {
                intensive_gauge_gated(family, &mut out, "kv_detail", |s, v| {
                    s.kv_usage_detail.insert("mamba", v);
                });
            }
            // Auxiliary queue depths (extensive: `pid`-fanned children sum;
            // multiple live scheduler processes each hold real queue slots).
            "sglang:num_grammar_queue_reqs" => {
                extensive_gauge(family, &mut out, "waiting_extras", |s, v| {
                    s.waiting_extras.insert("grammar", v);
                });
            }
            "sglang:num_paused_reqs" => {
                extensive_gauge(family, &mut out, "waiting_extras", |s, v| {
                    s.waiting_extras.insert("paused", v);
                });
            }
            "sglang:num_retracted_reqs" => {
                extensive_gauge(family, &mut out, "waiting_extras", |s, v| {
                    s.waiting_extras.insert("retracted", v);
                });
            }
            // PD queue gauges: meaningful only on disaggregated servers;
            // unified servers register them permanently at 0. The setter
            // filter below keeps them off unified series.
            "sglang:num_prefill_bootstrap_queue_reqs" => {
                pd_gauge(family, &mut out, |s, v| s.pd_bootstrap_queue = Some(v));
            }
            "sglang:num_prefill_inflight_queue_reqs" => {
                pd_gauge(family, &mut out, |s, v| s.pd_inflight_queue = Some(v));
            }
            "sglang:num_decode_prealloc_queue_reqs" => {
                pd_gauge(family, &mut out, |s, v| s.pd_prealloc_queue = Some(v));
            }
            "sglang:num_decode_transfer_queue_reqs" => {
                pd_gauge(family, &mut out, |s, v| s.pd_transfer_queue = Some(v));
            }

            // --- Scheduler counters ---
            "sglang:realtime_tokens_total" => {
                map_counter(family, &mut out, "realtime_tokens", "mode", |s, k, v| {
                    *s.realtime_tokens_by_mode.entry(k).or_insert(0.0) += v;
                });
            }
            "sglang:num_retracted_requests_total" => {
                plain_counter(family, &mut out, "retractions", |s, v| {
                    *s.retracted.get_or_insert(0.0) += v
                });
            }

            // --- Tokenizer counters ---
            "sglang:prompt_tokens_total" => {
                map_counter(
                    family,
                    &mut out,
                    "prompt_tokens",
                    "is_streaming",
                    |s, k, v| {
                        *s.prompt_tokens_by_stream.entry(k).or_insert(0.0) += v;
                    },
                );
            }
            "sglang:generation_tokens_total" => {
                map_counter(
                    family,
                    &mut out,
                    "generation_tokens",
                    "is_streaming",
                    |s, k, v| {
                        *s.generation_tokens_by_stream.entry(k).or_insert(0.0) += v;
                    },
                );
            }
            "sglang:num_requests_total" => {
                map_counter(family, &mut out, "requests", "is_streaming", |s, k, v| {
                    *s.requests_by_stream.entry(k).or_insert(0.0) += v;
                });
            }
            "sglang:num_aborted_requests_total" => {
                plain_counter(family, &mut out, "aborts", |s, v| {
                    *s.aborted.get_or_insert(0.0) += v
                });
            }
            "sglang:cached_tokens_total" => {
                map_counter(
                    family,
                    &mut out,
                    "cached_tokens",
                    "cache_source",
                    |s, k, v| {
                        *s.cached_tokens_by_source.entry(k).or_insert(0.0) += v;
                    },
                );
            }

            // --- Histograms ---
            STAGE_HISTOGRAM => {
                curate_stage_histogram(family, &mut out);
            }
            KV_TRANSFER_HISTOGRAM_MS => {
                curate_histogram_scaled(family, &mut out, hist::KV_TRANSFER_LATENCY, 1e-3);
            }
            _ => {
                if let Some((canonical, _)) = HISTOGRAM_SPECS
                    .iter()
                    .find(|(_, names)| names.contains(&family.name.as_str()))
                {
                    curate_histogram_scaled(family, &mut out, canonical, 1.0);
                }
            }
        }
    }
    out
}

fn push_capability(caps: &mut Vec<&'static str>, cap: &'static str) {
    if !caps.contains(&cap) {
        caps.push(cap);
    }
}

/// Intensive gauge (a level/ratio): within each series' minimum rank tuple,
/// take the value when all samples agree; disagreement (e.g. stale
/// multiprocess children) yields absent rather than a silent average.
fn intensive_gauge(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    set: impl Fn(&mut CuratedSeries, f64),
) {
    intensive_gauge_inner(family, out, cap, set, false);
}

/// Intensive gauge that is registered-at-zero upstream: values <= 0 mean
/// "feature off" and curate as absent.
fn intensive_gauge_gated(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    set: impl Fn(&mut CuratedSeries, f64),
) {
    intensive_gauge_inner(family, out, cap, set, true);
}

fn intensive_gauge_inner(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    set: impl Fn(&mut CuratedSeries, f64),
    positive_only: bool,
) {
    let mut any = false;
    for (key, samples) in rank_projected(family) {
        let mut agreed: Option<f64> = None;
        let mut conflict = false;
        for s in &samples {
            if !s.value.is_finite() {
                continue;
            }
            match agreed {
                None => agreed = Some(s.value),
                Some(v) if v == s.value => {}
                Some(_) => conflict = true,
            }
        }
        let Some(v) = agreed else { continue };
        if conflict || (positive_only && v <= 0.0) {
            continue;
        }
        set(out.series.entry(key).or_default(), v);
        any = true;
    }
    if any {
        push_capability(&mut out.capabilities, cap);
    }
}

/// Extensive gauge (a count of things): residual children (`pid`) sum.
fn extensive_gauge(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    set: impl Fn(&mut CuratedSeries, f64),
) {
    let mut any = false;
    for (key, samples) in rank_projected(family) {
        let total: f64 = samples
            .iter()
            .map(|s| s.value)
            .filter(|v| v.is_finite())
            .sum();
        if samples.iter().all(|s| !s.value.is_finite()) {
            continue;
        }
        set(out.series.entry(key).or_default(), total);
        any = true;
    }
    if any {
        push_capability(&mut out.capabilities, cap);
    }
}

/// PD queue gauge: extensive, but kept only on prefill/decode series —
/// unified servers register these permanently at zero.
fn pd_gauge(family: &MetricFamily, out: &mut CuratedScrape, set: impl Fn(&mut CuratedSeries, f64)) {
    let mut any = false;
    for (key, samples) in rank_projected(family) {
        if key.engine_type.is_none() {
            continue;
        }
        if samples.iter().all(|s| !s.value.is_finite()) {
            continue; // all-NaN must stay absent, never sum to a fake 0.0
        }
        let total: f64 = samples
            .iter()
            .map(|s| s.value)
            .filter(|v| v.is_finite())
            .sum();
        set(out.series.entry(key).or_default(), total);
        any = true;
    }
    if any {
        push_capability(&mut out.capabilities, "pd_queues");
    }
}

/// Plain cumulative counter: residual children sum (additive and monotone as
/// long as children only appear or grow; restarts reset all together).
fn plain_counter(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    add: impl Fn(&mut CuratedSeries, f64),
) {
    let mut any = false;
    for (key, samples) in rank_projected(family) {
        let mut found = false;
        let mut total = 0.0;
        for s in &samples {
            if s.value.is_finite() {
                total += s.value;
                found = true;
            }
        }
        if !found {
            continue;
        }
        add(out.series.entry(key).or_default(), total);
        any = true;
    }
    if any {
        push_capability(&mut out.capabilities, cap);
    }
}

/// Counter fanned out over one routing label (`mode`, `is_streaming`,
/// `cache_source`): route by that label, sum any remaining residuals.
///
/// Samples that lack the routing label are routed to [`UNROUTED`] rather than
/// dropped — a server that does not fan a family out (older builds, or a
/// future one that retires a label) still has real counters, and discarding
/// them would silently blank the lifetime totals and every rate derived from
/// them.
fn map_counter(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    cap: &'static str,
    label: &str,
    insert: impl Fn(&mut CuratedSeries, String, f64),
) {
    let mut any = false;
    for (key, samples) in rank_projected(family) {
        let series = out.series.entry(key).or_default();
        for s in samples {
            let routed = s.labels.get(label).unwrap_or(UNROUTED);
            if !s.value.is_finite() {
                continue;
            }
            insert(series, routed.to_string(), s.value);
            any = true;
        }
    }
    if any {
        push_capability(&mut out.capabilities, cap);
    }
}

/// Collect one histogram family into a canonical id, merging residual
/// sub-series (`is_streaming`) by bucket-wise SUM of cumulative counts and
/// `+=` on sum/count. Children of one family share bucket layout by
/// construction, so per-`le` addition is exact — assignment instead of
/// addition here silently halves percentile inputs (the bug this replaces).
/// `scale` converts bucket bounds and sums into seconds (1e-3 for ms).
fn curate_histogram_scaled(
    family: &MetricFamily,
    out: &mut CuratedScrape,
    canonical: &'static str,
    scale: f64,
) {
    if family.kind != MetricType::Histogram {
        return;
    }
    let grouped = histogram_groups(family, scale, |_| Some(canonical));
    let mut any = false;
    for (key, by_id) in grouped {
        for (id, histogram) in by_id {
            if histogram.buckets.is_empty() {
                continue;
            }
            out.series
                .entry(key.clone())
                .or_default()
                .histograms
                .insert(id, histogram);
            any = true;
        }
    }
    if any {
        push_capability(&mut out.capabilities, canonical);
    }
}

/// `sglang:per_stage_req_latency_seconds`: the `stage` label routes each
/// sub-series to its own canonical id from the [`hist::STAGES`] allowlist.
fn curate_stage_histogram(family: &MetricFamily, out: &mut CuratedScrape) {
    if family.kind != MetricType::Histogram {
        return;
    }
    let grouped = histogram_groups(family, 1.0, |labels| {
        let stage = labels.get("stage")?;
        hist::STAGES
            .iter()
            .find(|(label, _)| *label == stage)
            .map(|(_, id)| *id)
    });
    let mut ids_found: Vec<&'static str> = Vec::new();
    for (key, by_id) in grouped {
        for (id, histogram) in by_id {
            if histogram.buckets.is_empty() {
                continue;
            }
            out.series
                .entry(key.clone())
                .or_default()
                .histograms
                .insert(id, histogram);
            if !ids_found.contains(&id) {
                ids_found.push(id);
            }
        }
    }
    for id in ids_found {
        push_capability(&mut out.capabilities, id);
    }
}

/// Shared histogram grouping: rank-project, route each sample to a canonical
/// id, and merge by summing per-`le` bucket counts and accumulating
/// sum/count. `scale` multiplies `le` bounds and `_sum` (unit conversion).
fn histogram_groups(
    family: &MetricFamily,
    scale: f64,
    route: impl Fn(&super::model::LabelSet) -> Option<&'static str>,
) -> BTreeMap<SeriesKey, BTreeMap<&'static str, RawHistogram>> {
    let bucket_name = format!("{}_bucket", family.name);
    let sum_name = format!("{}_sum", family.name);
    let count_name = format!("{}_count", family.name);

    let mut grouped: BTreeMap<SeriesKey, BTreeMap<&'static str, RawHistogram>> = BTreeMap::new();
    for (key, samples) in rank_projected(family) {
        let by_id = grouped.entry(key).or_default();
        for sample in samples {
            let Some(id) = route(&sample.labels) else {
                continue;
            };
            if !sample.value.is_finite() && sample.name == bucket_name {
                continue;
            }
            let entry = by_id.entry(id).or_default();
            if sample.name == bucket_name {
                if let Some(le) = sample.labels.get("le").and_then(parse_le) {
                    let le = if le.is_finite() { le * scale } else { le };
                    // Merge residual sub-series (e.g. is_streaming) by
                    // summing cumulative counts at identical bounds.
                    match entry.buckets.iter_mut().find(|(b, _)| *b == le) {
                        Some((_, count)) => *count += sample.value,
                        None => entry.buckets.push((le, sample.value)),
                    }
                }
            } else if sample.name == sum_name {
                entry.sum += sample.value * scale;
            } else if sample.name == count_name {
                entry.count += sample.value;
            }
        }
    }
    grouped
}

fn parse_le(raw: &str) -> Option<f64> {
    match raw {
        "+Inf" | "+inf" | "inf" | "Inf" => Some(f64::INFINITY),
        _ => raw.parse::<f64>().ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::parse::parse_text;

    const BASE: &str =
        r#"engine_type="unified",model_name="m1",moe_ep_rank="0",pp_rank="0",tp_rank="0""#;

    fn snippet() -> String {
        format!(
            r#"
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{{{BASE}}} 4.0
# TYPE sglang:num_queue_reqs gauge
sglang:num_queue_reqs{{{BASE}}} 1.0
# TYPE sglang:token_usage gauge
sglang:token_usage{{{BASE}}} 0.25
# TYPE sglang:num_used_tokens gauge
sglang:num_used_tokens{{{BASE}}} 32754.0
# TYPE sglang:max_total_num_tokens gauge
sglang:max_total_num_tokens{{{BASE}}} 131016.0
# TYPE sglang:cache_hit_rate gauge
sglang:cache_hit_rate{{{BASE}}} 0.6
# TYPE sglang:gen_throughput gauge
sglang:gen_throughput{{{BASE}}} 123.0
# TYPE sglang:spec_accept_length gauge
sglang:spec_accept_length{{{BASE}}} 3.3
# TYPE sglang:spec_accept_rate gauge
sglang:spec_accept_rate{{{BASE}}} 0.33
# TYPE sglang:realtime_tokens_total counter
sglang:realtime_tokens_total{{{BASE},mode="prefill_compute"}} 29215.0
sglang:realtime_tokens_total{{{BASE},mode="prefill_cache"}} 44096.0
sglang:realtime_tokens_total{{{BASE},mode="decode"}} 14220.0
# TYPE sglang:prompt_tokens_total counter
sglang:prompt_tokens_total{{engine_type="unified",is_streaming="false",model_name="m1"}} 486.0
sglang:prompt_tokens_total{{engine_type="unified",is_streaming="true",model_name="m1"}} 72825.0
# TYPE sglang:generation_tokens_total counter
sglang:generation_tokens_total{{engine_type="unified",is_streaming="true",model_name="m1"}} 14079.0
# TYPE sglang:num_requests_total counter
sglang:num_requests_total{{engine_type="unified",is_streaming="false",model_name="m1"}} 2.0
sglang:num_requests_total{{engine_type="unified",is_streaming="true",model_name="m1"}} 3.0
# TYPE sglang:num_aborted_requests_total counter
sglang:num_aborted_requests_total{{engine_type="unified",model_name="m1"}} 1.0
# TYPE sglang:cached_tokens_total counter
sglang:cached_tokens_total{{cache_source="device",engine_type="unified",model_name="m1"}} 44096.0
# TYPE sglang:time_to_first_token_seconds histogram
sglang:time_to_first_token_seconds_bucket{{engine_type="unified",is_streaming="false",le="0.5",model_name="m1"}} 1.0
sglang:time_to_first_token_seconds_bucket{{engine_type="unified",is_streaming="false",le="+Inf",model_name="m1"}} 2.0
sglang:time_to_first_token_seconds_count{{engine_type="unified",is_streaming="false",model_name="m1"}} 2.0
sglang:time_to_first_token_seconds_sum{{engine_type="unified",is_streaming="false",model_name="m1"}} 3.25
sglang:time_to_first_token_seconds_bucket{{engine_type="unified",is_streaming="true",le="0.5",model_name="m1"}} 0.0
sglang:time_to_first_token_seconds_bucket{{engine_type="unified",is_streaming="true",le="+Inf",model_name="m1"}} 3.0
sglang:time_to_first_token_seconds_count{{engine_type="unified",is_streaming="true",model_name="m1"}} 3.0
sglang:time_to_first_token_seconds_sum{{engine_type="unified",is_streaming="true",model_name="m1"}} 18.6
"#
        )
    }

    fn key() -> SeriesKey {
        SeriesKey {
            model: "m1".into(),
            engine_type: None,
            dp_rank: None,
        }
    }

    #[test]
    fn unified_engine_type_normalizes_out_of_the_key() {
        let curated = curate(&parse_text(&snippet()));
        assert_eq!(curated.series.len(), 1, "one merged series expected");
        let s = &curated.series[&key()];
        assert_eq!(s.running, Some(4.0));
        assert_eq!(s.waiting, Some(1.0));
        assert_eq!(s.kv_cache_usage, Some(0.25));
        assert_eq!(s.kv_used_tokens, Some(32754.0));
        assert_eq!(s.kv_cache_size_tokens, Some(131016.0));
        assert_eq!(key().display(), "m1");
        assert_eq!(key().engine_str(), None);
    }

    #[test]
    fn scheduler_and_tokenizer_metrics_merge_into_one_series() {
        let curated = curate(&parse_text(&snippet()));
        let s = &curated.series[&key()];
        // Scheduler side.
        assert_eq!(s.realtime_tokens_by_mode["decode"], 14220.0);
        assert_eq!(s.realtime_tokens_by_mode["prefill_compute"], 29215.0);
        // Tokenizer side, summed across is_streaming.
        assert_eq!(s.prompt_tokens_total(), Some(73311.0));
        assert_eq!(s.generation_tokens_total(), Some(14079.0));
        assert_eq!(s.requests_total(), Some(5.0));
        assert_eq!(s.aborted, Some(1.0));
        assert_eq!(s.cached_tokens_total(), Some(44096.0));
    }

    #[test]
    fn is_streaming_histograms_merge_by_bucket_sum_not_overwrite() {
        let curated = curate(&parse_text(&snippet()));
        let h = &curated.series[&key()].histograms[hist::TTFT];
        // 2 sub-series × 2 buckets → 2 merged buckets with summed counts.
        assert_eq!(h.buckets.len(), 2);
        let le_05 = h.buckets.iter().find(|(le, _)| *le == 0.5).unwrap().1;
        let le_inf = h.buckets.iter().find(|(le, _)| le.is_infinite()).unwrap().1;
        assert_eq!(le_05, 1.0); // 1.0 + 0.0
        assert_eq!(le_inf, 5.0); // 2.0 + 3.0
        assert_eq!(h.count, 5.0);
        assert!((h.sum - 21.85).abs() < 1e-9);
    }

    #[test]
    fn spec_gauges_are_value_gated() {
        let curated = curate(&parse_text(&snippet()));
        let s = &curated.series[&key()];
        assert_eq!(s.spec_accept_length, Some(3.3));
        assert_eq!(s.spec_accept_rate, Some(0.33));

        let off = format!(
            "# TYPE sglang:spec_accept_length gauge\nsglang:spec_accept_length{{{BASE}}} 0.0\n"
        );
        let curated = curate(&parse_text(&off));
        assert!(curated.series.is_empty(), "0.0 spec gauge curates nothing");
        assert!(!curated.has("spec"));
    }

    #[test]
    fn rank_projection_keeps_only_the_minimum_rank_tuple() {
        // --enable-metrics-for-all-schedulers: tp_rank 0 and 1 both export.
        let text = r#"
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{engine_type="unified",model_name="m",tp_rank="0",pp_rank="0",moe_ep_rank="0"} 4.0
sglang:num_running_reqs{engine_type="unified",model_name="m",tp_rank="1",pp_rank="0",moe_ep_rank="0"} 4.0
# TYPE sglang:realtime_tokens_total counter
sglang:realtime_tokens_total{engine_type="unified",model_name="m",tp_rank="0",pp_rank="0",moe_ep_rank="0",mode="decode"} 100.0
sglang:realtime_tokens_total{engine_type="unified",model_name="m",tp_rank="1",pp_rank="0",moe_ep_rank="0",mode="decode"} 100.0
"#;
        let curated = curate(&parse_text(text));
        let s = &curated.series[&SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        }];
        assert_eq!(s.running, Some(4.0), "not 8.0: ranks must not sum");
        assert_eq!(
            s.realtime_tokens_by_mode["decode"], 100.0,
            "not 200.0: ranks must not sum"
        );
    }

    #[test]
    fn dp_rank_fans_out_series_and_tokenizer_stays_frontend() {
        let text = r#"
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{engine_type="unified",model_name="m",dp_rank="0",tp_rank="0"} 2.0
sglang:num_running_reqs{engine_type="unified",model_name="m",dp_rank="1",tp_rank="0"} 3.0
# TYPE sglang:prompt_tokens_total counter
sglang:prompt_tokens_total{engine_type="unified",is_streaming="true",model_name="m"} 500.0
"#;
        let curated = curate(&parse_text(text));
        assert_eq!(curated.series.len(), 3);
        let dp = |d: &str| SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: Some(d.into()),
        };
        assert_eq!(curated.series[&dp("0")].running, Some(2.0));
        assert_eq!(curated.series[&dp("1")].running, Some(3.0));
        let frontend = SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        };
        assert_eq!(curated.series[&frontend].prompt_tokens_total(), Some(500.0));
        assert_eq!(dp("1").display(), "m [dp1]");
        assert_eq!(dp("1").engine_str(), Some("dp1".into()));
    }

    #[test]
    fn pd_queue_gauges_only_land_on_disaggregated_series() {
        let text = r#"
# TYPE sglang:num_prefill_bootstrap_queue_reqs gauge
sglang:num_prefill_bootstrap_queue_reqs{engine_type="unified",model_name="u"} 0.0
sglang:num_prefill_bootstrap_queue_reqs{engine_type="prefill",model_name="p"} 7.0
"#;
        let curated = curate(&parse_text(text));
        let prefill = SeriesKey {
            model: "p".into(),
            engine_type: Some("prefill".into()),
            dp_rank: None,
        };
        assert_eq!(curated.series[&prefill].pd_bootstrap_queue, Some(7.0));
        assert!(curated.has("pd_queues"));
        let unified = SeriesKey {
            model: "u".into(),
            engine_type: None,
            dp_rank: None,
        };
        assert!(!curated.series.contains_key(&unified));
        assert_eq!(prefill.display(), "p [prefill]");
        assert_eq!(prefill.engine_str(), Some("prefill".into()));
    }

    #[test]
    fn pid_fanned_extensive_gauges_sum_their_children() {
        let text = r#"
# TYPE sglang:num_retracted_reqs gauge
sglang:num_retracted_reqs{engine_type="unified",model_name="m",tp_rank="0",pid="100"} 2.0
sglang:num_retracted_reqs{engine_type="unified",model_name="m",tp_rank="0",pid="200"} 1.0
"#;
        let curated = curate(&parse_text(text));
        let s = &curated.series[&SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        }];
        assert_eq!(s.waiting_extras["retracted"], 3.0);
    }

    #[test]
    fn stage_histogram_routes_by_stage_label() {
        let text = r#"
# TYPE sglang:per_stage_req_latency_seconds histogram
sglang:per_stage_req_latency_seconds_bucket{model_name="m",stage="prefill_forward",le="0.1"} 5.0
sglang:per_stage_req_latency_seconds_bucket{model_name="m",stage="prefill_forward",le="+Inf"} 6.0
sglang:per_stage_req_latency_seconds_count{model_name="m",stage="prefill_forward"} 6.0
sglang:per_stage_req_latency_seconds_sum{model_name="m",stage="prefill_forward"} 0.9
sglang:per_stage_req_latency_seconds_bucket{model_name="m",stage="mystery_stage",le="+Inf"} 1.0
"#;
        let curated = curate(&parse_text(text));
        let s = &curated.series[&SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        }];
        let h = &s.histograms[hist::STAGE_PREFILL_FORWARD];
        assert_eq!(h.count, 6.0);
        assert!(curated.has(hist::STAGE_PREFILL_FORWARD));
        // Unknown stages are not curated (raw view only), and never panic.
        assert_eq!(s.histograms.len(), 1);
    }

    #[test]
    fn kv_transfer_latency_converts_ms_to_seconds() {
        let text = r#"
# TYPE sglang:kv_transfer_latency_ms histogram
sglang:kv_transfer_latency_ms_bucket{engine_type="decode",model_name="m",le="50"} 3.0
sglang:kv_transfer_latency_ms_bucket{engine_type="decode",model_name="m",le="+Inf"} 4.0
sglang:kv_transfer_latency_ms_count{engine_type="decode",model_name="m"} 4.0
sglang:kv_transfer_latency_ms_sum{engine_type="decode",model_name="m"} 120.0
"#;
        let curated = curate(&parse_text(text));
        let s = &curated.series[&SeriesKey {
            model: "m".into(),
            engine_type: Some("decode".into()),
            dp_rank: None,
        }];
        let h = &s.histograms[hist::KV_TRANSFER_LATENCY];
        assert_eq!(h.buckets.iter().find(|(le, _)| *le == 0.05).unwrap().1, 3.0);
        assert!((h.sum - 0.12).abs() < 1e-12);
        assert_eq!(h.count, 4.0);
    }

    #[test]
    fn legacy_alias_time_per_output_token() {
        let text = r#"
# TYPE sglang:time_per_output_token_seconds histogram
sglang:time_per_output_token_seconds_bucket{model_name="old",le="+Inf"} 5.0
sglang:time_per_output_token_seconds_count{model_name="old"} 5.0
sglang:time_per_output_token_seconds_sum{model_name="old"} 1.0
"#;
        let curated = curate(&parse_text(text));
        let key = SeriesKey {
            model: "old".into(),
            engine_type: None,
            dp_rank: None,
        };
        assert!(
            curated.series[&key]
                .histograms
                .contains_key(hist::INTER_TOKEN_LATENCY)
        );
    }

    #[test]
    fn nan_gauges_are_skipped_not_propagated() {
        // Live servers really do this: fwd_occupancy is NaN while idle.
        let text = r#"
# TYPE sglang:token_usage gauge
sglang:token_usage{model_name="m"} NaN
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{model_name="m"} 0.0
"#;
        let curated = curate(&parse_text(text));
        let s = &curated.series[&SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        }];
        assert_eq!(s.kv_cache_usage, None);
        assert_eq!(s.running, Some(0.0));
    }

    #[test]
    fn conflicting_intensive_gauge_values_curate_as_absent() {
        // Two children disagree (e.g. a stale multiprocess file): never
        // average, never pick one silently.
        let text = r#"
# TYPE sglang:token_usage gauge
sglang:token_usage{model_name="m",pid="1"} 0.2
sglang:token_usage{model_name="m",pid="2"} 0.9
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{model_name="m"} 1.0
"#;
        let curated = curate(&parse_text(text));
        let s = &curated.series[&SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        }];
        assert_eq!(s.kv_cache_usage, None);
    }

    #[test]
    fn all_nan_pd_gauge_stays_absent_instead_of_summing_to_zero() {
        // Live servers really do emit NaN gauges; an empty finite-filter must
        // not fall through to a fabricated 0.0 queue depth.
        let text = r#"
# TYPE sglang:num_prefill_bootstrap_queue_reqs gauge
sglang:num_prefill_bootstrap_queue_reqs{engine_type="prefill",model_name="p"} NaN
"#;
        let curated = curate(&parse_text(text));
        let key = SeriesKey {
            model: "p".into(),
            engine_type: Some("prefill".into()),
            dp_rank: None,
        };
        assert_eq!(
            curated.series.get(&key).and_then(|s| s.pd_bootstrap_queue),
            None
        );
        assert!(!curated.has("pd_queues"));
    }

    #[test]
    fn counters_without_their_routing_label_are_still_curated() {
        // A server that does not fan these families out over is_streaming
        // still has real counters; dropping them would blank every lifetime
        // total and every rate derived from them.
        let text = r#"
# TYPE sglang:prompt_tokens_total counter
sglang:prompt_tokens_total{engine_type="unified",model_name="m"} 486.0
# TYPE sglang:num_requests_total counter
sglang:num_requests_total{engine_type="unified",model_name="m"} 5.0
"#;
        let curated = curate(&parse_text(text));
        let s = &curated.series[&SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        }];
        assert_eq!(s.prompt_tokens_total(), Some(486.0));
        assert_eq!(s.requests_total(), Some(5.0));
        assert_eq!(s.prompt_tokens_by_stream[UNROUTED], 486.0);
        assert!(curated.has("prompt_tokens"));
    }

    #[test]
    fn missing_metrics_mean_empty_not_zero() {
        let curated = curate(&parse_text("up 1\n"));
        assert!(curated.series.is_empty());
        assert!(curated.capabilities.is_empty());
    }

    #[test]
    fn samples_without_model_label_are_ignored_for_curation() {
        let text = "# TYPE sglang:num_running_reqs gauge\nsglang:num_running_reqs 3.0\n";
        let curated = curate(&parse_text(text));
        assert!(curated.series.is_empty());
    }

    #[test]
    fn capability_detection_reports_what_was_found() {
        let curated = curate(&parse_text(&snippet()));
        assert!(curated.has("running"));
        assert!(curated.has("kv_capacity"));
        assert!(curated.has("cache_hit_rate"));
        assert!(curated.has("spec"));
        assert!(curated.has(hist::TTFT));
        assert!(!curated.has(hist::E2E_LATENCY));
        assert!(!curated.has("retractions"));
        assert!(!curated.has("pd_queues"));
    }
}

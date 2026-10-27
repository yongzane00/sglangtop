//! Application state: one `AppState` owned by the main loop, updated by
//! reducing collector events, read by the renderer. No locks, no sharing.

pub mod bursts;
pub mod events;
pub mod history;
pub mod requests;

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant, SystemTime};

use crate::metrics::histogram::{HistogramPoint, HistogramWindow, WindowEstimate};
use crate::metrics::model::ScrapeText;
use crate::metrics::normalize::{CuratedScrape, RawHistogram, SeriesKey, curate};
use crate::metrics::rates::{CounterBank, RateSample};
use history::RingSeries;

/// History series ids (per endpoint × series key).
pub mod series_id {
    pub const RUNNING: &str = "running";
    pub const WAITING: &str = "waiting";
    pub const KV_USAGE: &str = "kv_usage";
    pub const PROMPT_TPS: &str = "prompt_tps";
    pub const GENERATION_TPS: &str = "generation_tps";
    pub const REQUEST_RATE: &str = "request_rate";
    pub const CACHE_HIT_RATE: &str = "cache_hit_rate";
    pub const TTFT_P50: &str = "ttft_p50";
    pub const TTFT_P95: &str = "ttft_p95";
    pub const ITL_P95: &str = "itl_p95";
    pub const E2E_P95: &str = "e2e_p95";
    pub const ABORTS: &str = "aborts";
    pub const RETRACTIONS: &str = "retractions";
    pub const SPEC_ACCEPT_RATE: &str = "spec_accept_rate";

    /// The chart-ring set: ids with in-memory history, recorded at 1 Hz.
    pub const ALL: &[&str] = &[
        RUNNING,
        WAITING,
        KV_USAGE,
        PROMPT_TPS,
        GENERATION_TPS,
        REQUEST_RATE,
        CACHE_HIT_RATE,
        TTFT_P50,
        TTFT_P95,
        ITL_P95,
        E2E_P95,
        ABORTS,
        RETRACTIONS,
        SPEC_ACCEPT_RATE,
    ];

    // Cumulative counter snapshots, recorded (throttled) for the daily-usage
    // charts. Deliberately NOT in `ALL`: they get no in-memory ring.
    pub const PROMPT_TOKENS_TOTAL: &str = "prompt_tokens_total";
    pub const GENERATION_TOKENS_TOTAL: &str = "generation_tokens_total";
    pub const REQUESTS_TOTAL: &str = "requests_total";

    /// Single source of truth for the usage query's metric list.
    pub const CUMULATIVE: &[&str] = &[PROMPT_TOKENS_TOTAL, GENERATION_TOKENS_TOTAL, REQUESTS_TOTAL];
}

/// Consecutive scrapes a series may be missing before its history rings and
/// histogram windows are dropped. Long enough to ride out a transient shape
/// change (a `/v1/loads` fallback scrape, a reshaped response), short enough
/// that genuine label churn cannot grow memory without bound.
const SERIES_EVICT_AFTER_SCRAPES: u32 = 16;

/// What a collector reports after one poll cycle.
#[derive(Debug)]
pub struct ScrapeOutcome {
    /// Monotonic completion time of the scrape.
    pub at: Instant,
    /// Wall-clock completion time (display/recording only).
    pub wall: SystemTime,
    /// How long the `/metrics` request took.
    pub duration: Duration,
    pub result: Result<ScrapePayload, String>,
}

/// Successful poll payload. Optional endpoints may be probed less often;
/// `None` means "not checked this round", not "absent".
#[derive(Debug, Default)]
pub struct ScrapePayload {
    pub metrics: Option<ScrapeText>,
    pub healthy: Option<bool>,
    pub server_info: Option<ServerInfo>,
    pub models: Option<Vec<ServedModel>>,
    /// `/v1/loads` snapshot, shipped INSTEAD of `metrics` when the server
    /// has `--enable-metrics` off (never both — no dual-source merging).
    pub loads: Option<crate::metrics::loads::LoadsResponse>,
}

/// Static server configuration from SGLang's `/server_info` endpoint
/// (ServerArgs + version). Deserialized defensively: every field optional,
/// unknown fields ignored — the endpoint returns hundreds of fields whose set
/// varies by version, and we must never fail the probe over a missing one.
///
/// Metadata only: refreshed at the slow metadata cadence, displayed in the
/// endpoint header/pulse strip, never charted or recorded.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    pub version: Option<String>,
    pub max_running_requests: Option<u64>,
    pub max_total_num_tokens: Option<u64>,
    /// `None`/absent when speculative decoding is off; an algorithm name
    /// (e.g. "DFLASH", "EAGLE") when on.
    pub speculative_algorithm: Option<String>,
    /// SGLang encodes "not disaggregated" as the *string* "null"; see
    /// [`ServerInfo::pd_role`].
    pub disaggregation_mode: Option<String>,
    /// Whether the server was launched with `--enable-metrics`. `false`
    /// upgrades a `/metrics` 404 into targeted guidance.
    pub enable_metrics: Option<bool>,
    pub dp_size: Option<u32>,
    pub tp_size: Option<u32>,
    pub served_model_name: Option<String>,
}

impl ServerInfo {
    /// The PD-disaggregation role, when the server runs disaggregated.
    /// Filters out SGLang's literal `"null"` string for the unified mode.
    pub fn pd_role(&self) -> Option<&str> {
        match self.disaggregation_mode.as_deref() {
            Some("prefill") => Some("prefill"),
            Some("decode") => Some("decode"),
            _ => None,
        }
    }
}

/// One model card from `/v1/models`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedModel {
    /// The served name.
    pub id: String,
    /// Underlying HF repo id or local filesystem path, when reported.
    pub root: Option<String>,
    /// Context window. SGLang also exports this as `sglang:context_len`.
    pub max_model_len: Option<u64>,
}

/// Windowed/derived values for one series.
#[derive(Debug, Clone, Default)]
pub struct DerivedSeries {
    /// Prefill tokens/s: rate of `realtime_tokens_total{prefill_compute}` +
    /// `{prefill_cache}` — the per-batch scheduler counters, genuinely live.
    pub prompt_tps: Option<f64>,
    /// Decode tokens/s from `realtime_tokens_total{decode}`; when that
    /// counter family is absent, the server's own `gen_throughput` gauge is
    /// used instead and flagged via `generation_tps_is_server_est`.
    pub generation_tps: Option<f64>,
    pub generation_tps_is_server_est: bool,
    /// Completions per second, summed over `is_streaming` children with
    /// valid intervals (finish-time semantics, like the counter itself).
    pub request_rate: Option<f64>,
    /// New aborted requests in the last interval.
    pub abort_delta: Option<f64>,
    /// New retractions (decode-OOM requeues) in the last interval.
    pub retraction_delta: Option<f64>,
    pub retraction_rate: Option<f64>,
    /// Windowed cached-token share: Δcached_tokens / Δprompt_tokens, both
    /// attributed at request finish, clamped to 0..=1.
    pub cached_token_share_window: Option<f64>,
    /// Any curated counter for this series went backwards this scrape.
    pub reset_detected: bool,
    /// Histogram estimates keyed by canonical id (`metrics::normalize::hist`).
    pub estimates: BTreeMap<&'static str, WindowEstimate>,
}

/// Connection status, separate from data staleness.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnStatus {
    #[default]
    NeverConnected,
    Connected,
    Failing {
        error: String,
        consecutive: u32,
    },
}

/// How fresh an endpoint's data is, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Recent successful scrape.
    Fresh,
    /// Last good data is older than the staleness threshold; the values shown
    /// are the preserved last-good snapshot.
    Stale,
    /// No successful scrape yet.
    Never,
}

#[derive(Debug)]
pub struct EndpointState {
    pub name: String,
    /// URL safe for display (no userinfo/query).
    pub display_url: String,
    pub status: ConnStatus,
    pub server_version: Option<String>,
    /// Last `/server_info` snapshot (metadata cadence; None until probed).
    pub server_info: Option<ServerInfo>,
    pub served_models: Vec<ServedModel>,
    /// `/health` result: None = never checked / unknown.
    pub healthy: Option<bool>,
    /// Monotonic start time of the most recent `/metrics` attempt.
    pub last_attempt_at: Option<Instant>,
    /// Present only while that endpoint's non-overlapping `/metrics` request
    /// is in flight.
    pub scraping_since: Option<Instant>,
    pub last_ok_at: Option<Instant>,
    pub last_ok_wall: Option<SystemTime>,
    pub last_scrape_duration: Option<Duration>,
    pub total_scrapes: u64,
    pub total_failures: u64,
    pub parse_issue_count: usize,
    /// Last good curated extraction, preserved while stale.
    pub curated: Option<CuratedScrape>,
    pub derived: BTreeMap<SeriesKey, DerivedSeries>,
    /// Session peaks of the endpoint-aggregate token rates, reset when the
    /// monitored server restarts (a peak from before a restart would be a
    /// stale brag).
    pub peak_generation_tps: Option<f64>,
    pub peak_prompt_tps: Option<f64>,
    /// Set when a counter reset was seen (vLLM restart); cleared after display.
    pub restart_seen_at: Option<Instant>,
    /// Rolling history per (series, metric id).
    pub history: HashMap<(SeriesKey, &'static str), RingSeries>,
    /// Recent requests from the (opt-in) log tailer. Prompt previews live
    /// only here — see `state::requests` for the privacy firewall.
    pub requests: requests::RequestLog,
    /// The prompts of the requests that finished, one entry per scrape
    /// interval that showed new prompt tokens; see `state::bursts`.
    pub bursts: bursts::BurstLog,
    /// Things sglangtop observed about this endpoint (connect, fail,
    /// restart, retraction, aborts). Bounded; see `state::events`.
    pub events: events::EventLog,

    counters: CounterBank<(SeriesKey, String)>,
    windows: HashMap<(SeriesKey, &'static str), HistogramWindow>,
    /// Consecutive scrapes in which a series with retained history/windows
    /// was absent; drives the grace-period eviction in `ingest_curated`.
    missing_scrapes: HashMap<SeriesKey, u32>,
    history_window: Duration,
    percentile_window: Duration,
}

impl EndpointState {
    pub fn new(
        name: String,
        display_url: String,
        history_window: Duration,
        percentile_window: Duration,
    ) -> Self {
        EndpointState {
            name,
            display_url,
            status: ConnStatus::default(),
            server_version: None,
            server_info: None,
            served_models: Vec::new(),
            healthy: None,
            last_attempt_at: None,
            scraping_since: None,
            last_ok_at: None,
            last_ok_wall: None,
            last_scrape_duration: None,
            total_scrapes: 0,
            total_failures: 0,
            parse_issue_count: 0,
            curated: None,
            derived: BTreeMap::new(),
            peak_generation_tps: None,
            peak_prompt_tps: None,
            restart_seen_at: None,
            history: HashMap::new(),
            requests: requests::RequestLog::default(),
            bursts: bursts::BurstLog::new(history_window),
            events: events::EventLog::default(),
            counters: CounterBank::new(),
            windows: HashMap::new(),
            missing_scrapes: HashMap::new(),
            history_window,
            percentile_window,
        }
    }

    /// Data freshness relative to `now`, given the configured refresh interval.
    pub fn freshness(&self, now: Instant, refresh_interval: Duration) -> Freshness {
        match self.last_ok_at {
            None => Freshness::Never,
            Some(at) => {
                let threshold = staleness_threshold(refresh_interval);
                if now.saturating_duration_since(at) > threshold {
                    Freshness::Stale
                } else {
                    Freshness::Fresh
                }
            }
        }
    }

    /// Apply one scrape outcome.
    pub fn apply(&mut self, outcome: ScrapeOutcome) {
        self.scraping_since = None;
        self.total_scrapes += 1;
        match outcome.result {
            Err(error) => {
                self.total_failures += 1;
                let consecutive = match &self.status {
                    ConnStatus::Failing { consecutive, .. } => consecutive + 1,
                    _ => 1,
                };
                // Only the first failure of a run is an event; a flapping
                // endpoint must not flood the feed.
                if consecutive == 1 {
                    self.events
                        .push(outcome.at, events::EventKind::Failed, error.clone());
                }
                self.status = ConnStatus::Failing { error, consecutive };
                // Deliberately keep raw/curated/derived: last good snapshot
                // stays visible, flagged stale via freshness().
            }
            Ok(payload) => {
                match &self.status {
                    ConnStatus::Failing { consecutive, .. } => {
                        let n = *consecutive;
                        self.events.push(
                            outcome.at,
                            events::EventKind::Recovered,
                            format!("scraping again after {n} failed attempt(s)"),
                        );
                    }
                    // First ever success.
                    _ if self.last_ok_at.is_none() => {
                        self.events.push(
                            outcome.at,
                            events::EventKind::Connected,
                            "first successful scrape".to_string(),
                        );
                    }
                    _ => {}
                }
                self.status = ConnStatus::Connected;
                if let Some(healthy) = payload.healthy {
                    self.healthy = Some(healthy);
                }
                if let Some(info) = payload.server_info {
                    self.absorb_server_info(info);
                }
                if let Some(models) = payload.models {
                    self.served_models = models;
                }
                if let Some(text) = payload.metrics {
                    self.last_ok_at = Some(outcome.at);
                    self.last_ok_wall = Some(outcome.wall);
                    self.last_scrape_duration = Some(outcome.duration);
                    self.parse_issue_count = text.issue_count;
                    self.ingest_metrics(text, outcome.at);
                } else if let Some(loads) = payload.loads {
                    // Metrics-disabled fallback: /v1/loads gauges are real
                    // data — they refresh the freshness clock like a scrape.
                    self.last_ok_at = Some(outcome.at);
                    self.last_ok_wall = Some(outcome.wall);
                    self.last_scrape_duration = Some(outcome.duration);
                    self.parse_issue_count = 0;
                    self.ingest_loads(loads, outcome.at);
                }
            }
        }
    }

    pub fn mark_attempt_started(&mut self, at: Instant) {
        self.last_attempt_at = Some(at);
        self.scraping_since = Some(at);
    }

    pub fn apply_optional(
        &mut self,
        healthy: Option<bool>,
        server_info: Option<ServerInfo>,
        models: Option<Vec<ServedModel>>,
    ) {
        if let Some(healthy) = healthy {
            self.healthy = Some(healthy);
        }
        if let Some(info) = server_info {
            self.absorb_server_info(info);
        }
        if let Some(models) = models {
            self.served_models = models;
        }
    }

    fn absorb_server_info(&mut self, info: ServerInfo) {
        if let Some(version) = &info.version {
            self.server_version = Some(version.clone());
        }
        self.server_info = Some(info);
    }

    /// The `n/max` bar capacity: an explicit user-configured value wins over
    /// what the server reports via `/server_info`.
    pub fn effective_max_running(&self, configured: Option<u32>) -> Option<u64> {
        configured.map(u64::from).or_else(|| {
            self.server_info
                .as_ref()
                .and_then(|i| i.max_running_requests)
        })
    }

    fn ingest_metrics(&mut self, text: ScrapeText, at: Instant) {
        let curated = curate(&text);
        self.ingest_curated(curated, at);
    }

    /// `/v1/loads` fallback path. Snapshots carry no model name, so it is
    /// resolved from `/v1/models` or `/server_info`; until either responds
    /// (they run on their own cadence) a placeholder keys the series and the
    /// identity self-corrects on the next scrape.
    fn ingest_loads(&mut self, loads: crate::metrics::loads::LoadsResponse, at: Instant) {
        let model = self
            .served_models
            .first()
            .map(|m| m.id.clone())
            .or_else(|| {
                self.server_info
                    .as_ref()
                    .and_then(|i| i.served_model_name.clone())
            })
            .unwrap_or_else(|| "(model pending)".into());
        let curated = crate::metrics::loads::curate_loads(&loads, &model);
        self.ingest_curated(curated, at);
    }

    fn ingest_curated(&mut self, curated: CuratedScrape, at: Instant) {
        self.counters.begin_scrape();
        let mut derived: BTreeMap<SeriesKey, DerivedSeries> = BTreeMap::new();
        // Prompt tokens, and prompts, of the requests that finished this
        // interval, summed over series; `None` as soon as any series cannot
        // say.
        let mut landed_tokens: Option<f64> = Some(0.0);
        let mut landed_prompts: Option<f64> = Some(0.0);

        for (key, series) in &curated.series {
            let mut d = DerivedSeries::default();
            let mut resets = false;

            let mut rate_of = |metric: &str, value: Option<f64>| -> RateSample {
                match value {
                    Some(v) => {
                        let s = self
                            .counters
                            .update((key.clone(), metric.to_string()), at, v);
                        resets |= s.is_reset();
                        s
                    }
                    None => RateSample::Unavailable,
                }
            };

            // Live token rates from realtime_tokens_total{mode}. If ANY
            // mode's counter reset this interval, the family is inconsistent
            // — summing the survivors would fake a plausible-but-wrong rate.
            // Report unavailable instead; it recovers next interval.
            let mut prefill_tps: Option<f64> = None;
            let mut decode_tps: Option<f64> = None;
            let mut realtime_reset = false;
            for (mode, value) in &series.realtime_tokens_by_mode {
                let s = rate_of(&format!("realtime:{mode}"), Some(*value));
                realtime_reset |= s.is_reset();
                if let RateSample::Rate { per_sec, .. } = s {
                    match mode.as_str() {
                        // Cache-hit prompt tokens are still prompt tokens
                        // admitted this batch; compute + cache = prefill.
                        "prefill_compute" | "prefill_cache" => {
                            *prefill_tps.get_or_insert(0.0) += per_sec;
                        }
                        "decode" => *decode_tps.get_or_insert(0.0) += per_sec,
                        _ => {}
                    }
                }
            }
            if realtime_reset {
                prefill_tps = None;
                decode_tps = None;
            }
            d.prompt_tps = prefill_tps;
            d.generation_tps = decode_tps;
            // Only when the realtime counter family is entirely absent
            // (e.g. loads-only mode) fall back to the server's own decode
            // throughput gauge, flagged as a server-side estimate.
            if series.realtime_tokens_by_mode.is_empty()
                && let Some(v) = series.gen_throughput_server
            {
                d.generation_tps = Some(v);
                d.generation_tps_is_server_est = true;
            }

            // Completions: rate per is_streaming child, summed where
            // available, with the same family-consistency rule.
            let mut total_rate: Option<f64> = None;
            let mut finished_delta: Option<f64> = None;
            let mut requests_reset = false;
            for (streaming, value) in &series.requests_by_stream {
                let s = rate_of(&format!("requests:{streaming}"), Some(*value));
                requests_reset |= s.is_reset();
                if let RateSample::Rate { per_sec, delta } = s {
                    *total_rate.get_or_insert(0.0) += per_sec;
                    *finished_delta.get_or_insert(0.0) += delta;
                }
            }
            if requests_reset {
                total_rate = None;
                finished_delta = None;
            }
            d.request_rate = total_rate;

            d.abort_delta = rate_of("aborted", series.aborted).delta();

            let retraction = rate_of("retractions", series.retracted);
            d.retraction_rate = retraction.per_sec();
            d.retraction_delta = retraction.delta();

            // Windowed cached-token share: how much of the recently admitted
            // prompt volume was served from cache. Both counters increment
            // at request finish, so numerator and denominator are consistent.
            //
            // Tracked per child, not on the pre-summed totals: one child
            // resetting (label re-registration, a multiprocess child
            // restarting) while a sibling keeps growing leaves the SUM still
            // rising, so a summed tracker sees a valid delta and computes a
            // share from a denominator missing the reset child's traffic —
            // a plausible-but-wrong percentage, often clamped to a fake 100%.
            // Same family-consistency rule as the rates above.
            let mut cached = None;
            let mut prompt_delta = None;
            let mut share_reset = false;
            for (source, value) in &series.cached_tokens_by_source {
                let s = rate_of(&format!("cached:{source}"), Some(*value));
                share_reset |= s.is_reset();
                if let Some(delta) = s.delta() {
                    *cached.get_or_insert(0.0) += delta;
                }
            }
            for (streaming, value) in &series.prompt_tokens_by_stream {
                let s = rate_of(&format!("prompt:{streaming}"), Some(*value));
                share_reset |= s.is_reset();
                if let Some(delta) = s.delta() {
                    *prompt_delta.get_or_insert(0.0) += delta;
                }
            }
            d.cached_token_share_window = match (cached, prompt_delta) {
                _ if share_reset => None,
                (Some(c), Some(p)) if p > 0.0 => Some((c / p).clamp(0.0, 1.0)),
                _ => None,
            };

            // SGLang adds a request's whole prompt to `prompt_tokens_total`
            // and ticks `num_requests_total` in the same call, once, as the
            // request finishes: this interval's deltas are the prompts of
            // the requests that finished, and how many they were.
            landed_tokens = match (landed_tokens, prompt_delta) {
                (Some(a), Some(b)) if !share_reset => Some(a + b),
                _ => None,
            };
            landed_prompts = match (landed_prompts, finished_delta) {
                (Some(a), Some(b)) => Some(a + b),
                _ => None,
            };

            // Histogram windows.
            for (canonical, raw_histogram) in &series.histograms {
                if let Some(point) = to_point(raw_histogram, at) {
                    let window = self
                        .windows
                        .entry((key.clone(), canonical))
                        .or_insert_with(|| HistogramWindow::new(self.percentile_window));
                    window.push(point);
                    if let Some(estimate) = window.estimate() {
                        d.estimates.insert(canonical, estimate);
                    }
                }
            }

            d.reset_detected = resets;
            if resets {
                self.restart_seen_at = Some(at);
            }
            derived.insert(key.clone(), d);
        }
        self.counters.end_scrape();

        if let Some(tokens) = landed_tokens {
            self.bursts.push(at, tokens, landed_prompts);
        }

        // Age out series missing from this scrape instead of dropping them
        // immediately. A series can vanish for one scrape without the server
        // having restarted — a `/v1/loads` fallback scrape keys per DP rank
        // and has no frontend series at all, and any successful-but-empty
        // response would otherwise destroy every chart's history. Only after
        // SERIES_EVICT_AFTER_SCRAPES consecutive absences do the rings and
        // windows go, which still bounds memory under real label churn.
        // (Same grace-period idea as `CounterBank`'s generation eviction.)
        let absent: std::collections::BTreeSet<SeriesKey> = self
            .history
            .keys()
            .map(|(key, _)| key)
            .chain(self.windows.keys().map(|(key, _)| key))
            .filter(|key| !curated.series.contains_key(*key))
            .cloned()
            .collect();
        self.missing_scrapes.retain(|key, _| absent.contains(key));
        let mut stale: Vec<SeriesKey> = Vec::new();
        for key in absent {
            let seen = self.missing_scrapes.entry(key.clone()).or_insert(0);
            *seen += 1;
            if *seen > SERIES_EVICT_AFTER_SCRAPES {
                stale.push(key);
            }
        }
        for key in &stale {
            self.history.retain(|(k, _), _| k != key);
            self.windows.retain(|(k, _), _| k != key);
            self.missing_scrapes.remove(key);
        }

        self.derived = derived;
        self.curated = Some(curated);

        // Observations worth remembering, all from deltas just computed — no
        // extra scraping. These live in `ingest_curated` rather than
        // `ingest_metrics` so the /v1/loads fallback path raises them too.
        let retracted: f64 = self
            .derived
            .values()
            .filter_map(|d| d.retraction_delta)
            .sum();
        if retracted > 0.0 {
            self.events.push(
                at,
                events::EventKind::Preemption,
                format!("{retracted:.0} request(s) retracted (decode OOM)"),
            );
        }
        let aborted: f64 = self.derived.values().filter_map(|d| d.abort_delta).sum();
        if aborted > 0.0 {
            self.events.push(
                at,
                events::EventKind::Errors,
                format!("{aborted:.0} request(s) aborted"),
            );
        }

        // Track session peaks of the aggregate rates; a server restart
        // invalidates earlier peaks along with everything else cumulative.
        if self.derived.values().any(|d| d.reset_detected) {
            self.events.push(
                at,
                events::EventKind::Restarted,
                "counters reset — server restarted".to_string(),
            );
            self.peak_generation_tps = None;
            self.peak_prompt_tps = None;
        }
        let sum_rate = |get: fn(&DerivedSeries) -> Option<f64>| -> Option<f64> {
            self.derived
                .values()
                .filter_map(get)
                .fold(None, |acc: Option<f64>, v| Some(acc.unwrap_or(0.0) + v))
        };
        // A server-estimated throughput never feeds the session peak. It comes
        // from a gauge, and in loads-only mode there are no counters at all,
        // so a reset is structurally undetectable — a peak captured there
        // would keep bragging after a restart we can never observe.
        let estimated = self
            .derived
            .values()
            .any(|d| d.generation_tps_is_server_est);
        if !estimated && let Some(g) = sum_rate(|d| d.generation_tps) {
            self.peak_generation_tps = Some(self.peak_generation_tps.map_or(g, |p| p.max(g)));
        }
        if let Some(p) = sum_rate(|d| d.prompt_tps) {
            self.peak_prompt_tps = Some(self.peak_prompt_tps.map_or(p, |q| q.max(p)));
        }

        // Record history points from the same rows the recorder sees.
        let window = self.history_window;
        for (key, id, value) in self.current_samples() {
            self.history
                .entry((key, id))
                .or_insert_with(|| RingSeries::new(window))
                .push(at, value);
        }
    }

    /// The latest value for every (series, metric id) pair — the exact rows
    /// history charts and the SQLite recorder consume, so both always agree.
    pub fn current_samples(&self) -> Vec<(SeriesKey, &'static str, f64)> {
        let Some(curated) = &self.curated else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for (key, series) in &curated.series {
            let d = self.derived.get(key);
            let est =
                |h: &str| -> Option<f64> { d.and_then(|d| d.estimates.get(h)).and_then(|e| e.p95) };
            let est_p50 =
                |h: &str| -> Option<f64> { d.and_then(|d| d.estimates.get(h)).and_then(|e| e.p50) };
            let pairs: [(&'static str, Option<f64>); 14] = [
                (series_id::RUNNING, series.running),
                (series_id::WAITING, series.waiting),
                (series_id::KV_USAGE, series.kv_cache_usage),
                (series_id::PROMPT_TPS, d.and_then(|d| d.prompt_tps)),
                (series_id::GENERATION_TPS, d.and_then(|d| d.generation_tps)),
                (series_id::REQUEST_RATE, d.and_then(|d| d.request_rate)),
                (series_id::CACHE_HIT_RATE, series.cache_hit_rate_recent),
                (
                    series_id::TTFT_P50,
                    est_p50(crate::metrics::normalize::hist::TTFT),
                ),
                (
                    series_id::TTFT_P95,
                    est(crate::metrics::normalize::hist::TTFT),
                ),
                (
                    series_id::ITL_P95,
                    est(crate::metrics::normalize::hist::INTER_TOKEN_LATENCY),
                ),
                (
                    series_id::E2E_P95,
                    est(crate::metrics::normalize::hist::E2E_LATENCY),
                ),
                (series_id::ABORTS, d.and_then(|d| d.abort_delta)),
                (series_id::RETRACTIONS, d.and_then(|d| d.retraction_delta)),
                (series_id::SPEC_ACCEPT_RATE, series.spec_accept_rate),
            ];
            for (id, value) in pairs {
                if let Some(v) = value {
                    rows.push((key.clone(), id, v));
                }
            }
        }
        rows
    }

    /// Cumulative counter snapshots for the SQLite recorder ONLY — never fed
    /// into the in-memory chart rings (no ring allocation, no chart panel).
    /// The daily-usage query reconstructs per-day totals from these via
    /// restart-aware positive deltas, which integrated rates cannot provide.
    pub fn cumulative_samples(&self) -> Vec<(SeriesKey, &'static str, f64)> {
        let Some(curated) = &self.curated else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for (key, series) in &curated.series {
            let pairs: [(&'static str, Option<f64>); 3] = [
                (series_id::PROMPT_TOKENS_TOTAL, series.prompt_tokens_total()),
                (
                    series_id::GENERATION_TOKENS_TOTAL,
                    series.generation_tokens_total(),
                ),
                (series_id::REQUESTS_TOTAL, series.requests_total()),
            ];
            for (id, value) in pairs {
                if let Some(v) = value {
                    rows.push((key.clone(), id, v));
                }
            }
        }
        rows
    }

    /// Endpoint-level aggregate over all series (fleet view building block).
    pub fn aggregate(&self) -> EndpointAggregate {
        let mut agg = EndpointAggregate::default();
        let Some(curated) = &self.curated else {
            return agg;
        };
        let mut kv_pairs: Vec<(f64, Option<f64>)> = Vec::new();
        let mut hit_rates: Vec<f64> = Vec::new();
        for (key, series) in &curated.series {
            let derived = self.derived.get(key);
            sum_opt(&mut agg.running, series.running);
            sum_opt(&mut agg.waiting, series.waiting);
            if let Some(d) = derived {
                sum_opt(&mut agg.prompt_tps, d.prompt_tps);
                sum_opt(&mut agg.generation_tps, d.generation_tps);
                if d.generation_tps.is_some() && d.generation_tps_is_server_est {
                    agg.generation_tps_is_estimate = true;
                }
                sum_opt(&mut agg.request_rate, d.request_rate);
                sum_opt(&mut agg.abort_delta, d.abort_delta);
                sum_opt(&mut agg.retraction_delta, d.retraction_delta);
                if let Some(p95) = d
                    .estimates
                    .get(crate::metrics::normalize::hist::TTFT)
                    .and_then(|e| e.p95)
                {
                    agg.worst_ttft_p95 = Some(agg.worst_ttft_p95.map_or(p95, |w: f64| w.max(p95)));
                }
            }
            if let Some(usage) = series.kv_cache_usage {
                kv_pairs.push((usage, series.kv_cache_size_tokens));
            }
            if let Some(rate) = series.cache_hit_rate_recent {
                hit_rates.push(rate);
            }
        }
        agg.kv_usage = aggregate_kv(&kv_pairs);
        // Cache hit rate has no exposed weight; a plain mean, labelled as
        // such by the UI whenever more than one series contributes.
        if !hit_rates.is_empty() {
            agg.cache_hit_rate = Some(hit_rates.iter().sum::<f64>() / hit_rates.len() as f64);
        }
        agg.models = curated
            .series
            .keys()
            .map(|k| k.model.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        agg
    }
}

/// KV aggregation policy: capacity-weighted when every series exposes
/// capacity; otherwise an unweighted mean, explicitly labelled as such.
/// Percentages are never silently averaged as if globally meaningful.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KvAggregate {
    /// Weighted by KV capacity in tokens (comparable across series).
    CapacityWeighted(f64),
    /// Plain mean of per-series usage fractions; label it in the UI.
    UnweightedMean(f64),
}

impl KvAggregate {
    pub fn value(self) -> f64 {
        match self {
            KvAggregate::CapacityWeighted(v) | KvAggregate::UnweightedMean(v) => v,
        }
    }

    pub fn is_weighted(self) -> bool {
        matches!(self, KvAggregate::CapacityWeighted(_))
    }
}

pub fn aggregate_kv(pairs: &[(f64, Option<f64>)]) -> Option<KvAggregate> {
    if pairs.is_empty() {
        return None;
    }
    let total_capacity: Option<f64> = pairs.iter().map(|(_, c)| *c).sum();
    match total_capacity {
        Some(cap) if cap > 0.0 => {
            let weighted: f64 = pairs.iter().map(|(u, c)| u * c.unwrap_or(0.0)).sum::<f64>() / cap;
            Some(KvAggregate::CapacityWeighted(weighted))
        }
        _ => {
            let mean = pairs.iter().map(|(u, _)| u).sum::<f64>() / pairs.len() as f64;
            Some(KvAggregate::UnweightedMean(mean))
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EndpointAggregate {
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    pub prompt_tps: Option<f64>,
    pub generation_tps: Option<f64>,
    /// True when any contributing series used the server's own throughput
    /// gauge instead of counter-derived rates (rendered with a `~`).
    pub generation_tps_is_estimate: bool,
    pub request_rate: Option<f64>,
    pub abort_delta: Option<f64>,
    pub retraction_delta: Option<f64>,
    pub worst_ttft_p95: Option<f64>,
    pub kv_usage: Option<KvAggregate>,
    /// Unweighted mean of per-series server-window hit rates.
    pub cache_hit_rate: Option<f64>,
    pub models: Vec<String>,
}

fn sum_opt(acc: &mut Option<f64>, v: Option<f64>) {
    if let Some(v) = v {
        *acc = Some(acc.unwrap_or(0.0) + v);
    }
}

/// Fleet-wide roll-up across endpoints, for the fleet view's card band.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FleetAggregate {
    pub endpoints_total: usize,
    /// Fresh and not reporting unhealthy.
    pub endpoints_up: usize,
    pub endpoints_stale: usize,
    /// Never connected, or currently failing.
    pub endpoints_down: usize,
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    pub prompt_tps: Option<f64>,
    pub generation_tps: Option<f64>,
    /// True when any contributing endpoint used the server's own throughput
    /// gauge rather than counter-derived rates.
    pub generation_tps_is_estimate: bool,
    pub request_rate: Option<f64>,
    pub abort_delta: Option<f64>,
    pub kv_usage: Option<KvAggregate>,
    pub worst_ttft_p95: Option<f64>,
    /// Which endpoint owns `worst_ttft_p95` — a fleet number is only
    /// actionable if you know where to look.
    pub worst_ttft_endpoint: Option<String>,
}

/// Roll up the fleet. Counts sum; KV goes through [`aggregate_kv`] over the
/// FLATTENED fleet-wide (usage, capacity) pairs, so weighting happens at the
/// series level and never as a mean of means; latencies take the worst and
/// name the endpoint.
///
/// Freshness is supplied by the caller rather than computed here, which keeps
/// this clock-free and unit-testable. Only `Fresh` endpoints contribute
/// activity numbers — a stale snapshot must not inflate fleet totals — but
/// every endpoint is counted in the status tallies.
pub fn aggregate_fleet<'a>(
    endpoints: impl Iterator<Item = (&'a EndpointState, Freshness)>,
) -> FleetAggregate {
    let mut agg = FleetAggregate::default();
    let mut kv_pairs: Vec<(f64, Option<f64>)> = Vec::new();

    for (e, freshness) in endpoints {
        agg.endpoints_total += 1;
        match freshness {
            Freshness::Fresh if e.healthy != Some(false) => agg.endpoints_up += 1,
            Freshness::Fresh => agg.endpoints_down += 1,
            Freshness::Stale => agg.endpoints_stale += 1,
            Freshness::Never => agg.endpoints_down += 1,
        }
        if freshness != Freshness::Fresh {
            continue;
        }

        let e_agg = e.aggregate();
        sum_opt(&mut agg.running, e_agg.running);
        sum_opt(&mut agg.waiting, e_agg.waiting);
        sum_opt(&mut agg.prompt_tps, e_agg.prompt_tps);
        sum_opt(&mut agg.generation_tps, e_agg.generation_tps);
        sum_opt(&mut agg.request_rate, e_agg.request_rate);
        sum_opt(&mut agg.abort_delta, e_agg.abort_delta);
        // One estimated contributor taints the fleet total: it is no longer
        // purely counter-derived.
        agg.generation_tps_is_estimate |= e_agg.generation_tps_is_estimate;

        if let Some(worst) = e_agg.worst_ttft_p95
            && agg.worst_ttft_p95.is_none_or(|cur| worst > cur)
        {
            agg.worst_ttft_p95 = Some(worst);
            agg.worst_ttft_endpoint = Some(e.name.clone());
        }

        if let Some(curated) = &e.curated {
            for series in curated.series.values() {
                if let Some(usage) = series.kv_cache_usage {
                    kv_pairs.push((usage, series.kv_cache_size_tokens));
                }
            }
        }
    }

    agg.kv_usage = aggregate_kv(&kv_pairs);
    agg
}

fn to_point(raw: &RawHistogram, at: Instant) -> Option<HistogramPoint> {
    HistogramPoint::new(at, raw.buckets.clone(), raw.sum, raw.count)
}

/// Stale when no success for 3 intervals (min 5s) — tolerates one missed
/// scrape plus jitter without flapping.
pub fn staleness_threshold(refresh_interval: Duration) -> Duration {
    (refresh_interval * 3).max(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrape(at: Instant, secs_offset: f64, text: &str) -> ScrapeOutcome {
        ScrapeOutcome {
            at: at + Duration::from_secs_f64(secs_offset),
            wall: SystemTime::now(),
            duration: Duration::from_millis(10),
            result: Ok(ScrapePayload {
                metrics: Some(crate::metrics::parse::parse_text(text)),
                ..Default::default()
            }),
        }
    }

    /// `prompt`/`generated` drive both the realtime (rate-source) counters
    /// and the tokenizer lifetime counters; `finished_stream`/`finished_other`
    /// are the two `is_streaming` children of `num_requests_total`, and
    /// `finished_other` doubles as the abort counter.
    fn metrics_text(
        prompt: f64,
        generated: f64,
        finished_stream: f64,
        finished_other: f64,
    ) -> String {
        format!(
            r#"
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{{engine_type="unified",model_name="m",tp_rank="0"}} 2.0
# TYPE sglang:token_usage gauge
sglang:token_usage{{engine_type="unified",model_name="m",tp_rank="0"}} 0.4
# TYPE sglang:realtime_tokens_total counter
sglang:realtime_tokens_total{{engine_type="unified",model_name="m",tp_rank="0",mode="prefill_compute"}} {prompt}
sglang:realtime_tokens_total{{engine_type="unified",model_name="m",tp_rank="0",mode="decode"}} {generated}
# TYPE sglang:prompt_tokens_total counter
sglang:prompt_tokens_total{{engine_type="unified",is_streaming="true",model_name="m"}} {prompt}
# TYPE sglang:generation_tokens_total counter
sglang:generation_tokens_total{{engine_type="unified",is_streaming="true",model_name="m"}} {generated}
# TYPE sglang:num_requests_total counter
sglang:num_requests_total{{engine_type="unified",is_streaming="true",model_name="m"}} {finished_stream}
sglang:num_requests_total{{engine_type="unified",is_streaming="false",model_name="m"}} {finished_other}
# TYPE sglang:num_aborted_requests_total counter
sglang:num_aborted_requests_total{{engine_type="unified",model_name="m"}} {finished_other}
"#
        )
    }

    fn ep() -> EndpointState {
        EndpointState::new(
            "test".into(),
            "http://localhost:8000".into(),
            Duration::from_secs(300),
            Duration::from_secs(60),
        )
    }

    fn key() -> SeriesKey {
        SeriesKey {
            model: "m".into(),
            engine_type: None,
            dp_rank: None,
        }
    }

    #[test]
    fn prompt_bursts_come_from_finish_time_counter_deltas() {
        // SGLang bumps prompt_tokens_total and num_requests_total together
        // as each request finishes: the deltas are whole prompts and how
        // many requests they belonged to.
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        assert!(e.bursts.is_empty(), "no interval yet");
        e.apply(scrape(base, 1.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        assert!(e.bursts.is_empty(), "nothing finished");
        // One request with a 55.2k prompt, then two totalling 9.8k.
        e.apply(scrape(base, 2.0, &metrics_text(56_200.0, 600.0, 11.0, 0.0)));
        e.apply(scrape(base, 3.0, &metrics_text(66_000.0, 700.0, 12.0, 1.0)));
        let now = base + Duration::from_secs(3);
        let got: Vec<(f64, Option<u32>)> = e
            .bursts
            .recent(now)
            .map(|b| (b.tokens, b.requests))
            .collect();
        assert_eq!(got, vec![(9800.0, Some(2)), (55_200.0, Some(1))]);
        assert_eq!(e.bursts.total_tokens(now), 65_000.0);

        // Without the request counter the tokens are known, the count is not.
        let only_prompt = |prompt: f64| {
            format!(
                r#"
# TYPE sglang:prompt_tokens_total counter
sglang:prompt_tokens_total{{engine_type="unified",is_streaming="true",model_name="m"}} {prompt}
"#
            )
        };
        let mut e = ep();
        e.apply(scrape(base, 0.0, &only_prompt(1000.0)));
        e.apply(scrape(base, 2.0, &only_prompt(1200.0)));
        let got: Vec<(f64, Option<u32>)> = e
            .bursts
            .recent(base + Duration::from_secs(2))
            .map(|b| (b.tokens, b.requests))
            .collect();
        assert_eq!(got, vec![(200.0, None)]);
    }

    #[test]
    fn first_scrape_has_gauges_but_no_rates() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.prompt_tps, None);
        assert_eq!(d.request_rate, None);
        let c = &e.curated.as_ref().unwrap().series[&key()];
        assert_eq!(c.running, Some(2.0));
        assert_eq!(c.kv_cache_usage, Some(0.4));
    }

    #[test]
    fn second_scrape_produces_rates() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        e.apply(scrape(base, 2.0, &metrics_text(1200.0, 700.0, 14.0, 1.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.prompt_tps, Some(100.0));
        assert_eq!(d.generation_tps, Some(100.0));
        assert!(!d.generation_tps_is_server_est);
        // (14-10)/2 + (1-0)/2 = 2.5 completions/sec
        assert_eq!(d.request_rate, Some(2.5));
        assert_eq!(d.abort_delta, Some(1.0));
    }

    #[test]
    fn counter_reset_marks_restart_and_suppresses_rates() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(5000.0, 4000.0, 50.0, 0.0)));
        e.apply(scrape(base, 1.0, &metrics_text(100.0, 80.0, 1.0, 0.0)));
        let d = &e.derived[&key()];
        assert!(d.reset_detected);
        assert!(e.restart_seen_at.is_some());
        assert_eq!(d.prompt_tps, None);
        // Rates recover on the next interval.
        e.apply(scrape(base, 2.0, &metrics_text(200.0, 160.0, 2.0, 0.0)));
        assert_eq!(e.derived[&key()].prompt_tps, Some(100.0));
    }

    #[test]
    fn cumulative_samples_snapshot_counters_without_ring_history() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 2.0)));
        let rows = e.cumulative_samples();
        // Unlike rates, snapshots exist from the FIRST scrape.
        let get = |id: &str| rows.iter().find(|(_, m, _)| *m == id).map(|(_, _, v)| *v);
        assert_eq!(rows.len(), 3);
        assert_eq!(get(series_id::PROMPT_TOKENS_TOTAL), Some(1000.0));
        assert_eq!(get(series_id::GENERATION_TOKENS_TOTAL), Some(500.0));
        // stop + error finish reasons.
        assert_eq!(get(series_id::REQUESTS_TOTAL), Some(12.0));
        // And they never allocate in-memory chart rings.
        assert!(
            e.history
                .keys()
                .all(|(_, id)| !series_id::CUMULATIVE.contains(id))
        );
    }

    #[test]
    fn throughput_peaks_track_max_and_reset_on_server_restart() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        assert_eq!(e.peak_generation_tps, None); // no rate yet
        // 200 gen tokens/s, then a slower interval: peak sticks at the max.
        e.apply(scrape(base, 1.0, &metrics_text(1400.0, 700.0, 12.0, 0.0)));
        e.apply(scrape(base, 2.0, &metrics_text(1450.0, 750.0, 13.0, 0.0)));
        assert_eq!(e.peak_generation_tps, Some(200.0));
        assert_eq!(e.peak_prompt_tps, Some(400.0));
        // Server restart: counters fall back; stale peaks are discarded.
        e.apply(scrape(base, 3.0, &metrics_text(10.0, 5.0, 0.0, 0.0)));
        assert_eq!(e.peak_generation_tps, None);
        // New peaks accumulate from the post-restart world.
        e.apply(scrape(base, 4.0, &metrics_text(60.0, 30.0, 1.0, 0.0)));
        assert_eq!(e.peak_generation_tps, Some(25.0));
        assert_eq!(e.peak_prompt_tps, Some(50.0));
    }

    #[test]
    fn partial_reset_within_success_family_reports_unavailable_not_partial_sum() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 50.0, 10.0)));
        // The streaming child keeps growing but the non-streaming child
        // resets (e.g. label re-registration): summing only the survivor
        // would fake a plausible-but-wrong rate.
        e.apply(scrape(base, 1.0, &metrics_text(1100.0, 600.0, 60.0, 2.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.request_rate, None);
        assert_eq!(d.abort_delta, None);
        // Unrelated rates in other families are unaffected.
        assert_eq!(d.prompt_tps, Some(100.0));
        // Next interval recovers.
        e.apply(scrape(base, 2.0, &metrics_text(1200.0, 700.0, 70.0, 3.0)));
        let d = &e.derived[&key()];
        assert_eq!(d.request_rate, Some(11.0)); // (70-60) + (3-2)
        assert_eq!(d.abort_delta, Some(1.0));
    }

    #[test]
    fn partial_reset_within_prompt_family_suppresses_cached_share() {
        let base = Instant::now();
        let mut e = ep();
        let text = |streaming: f64, other: f64, cached: f64| {
            format!(
                r#"
# TYPE sglang:prompt_tokens_total counter
sglang:prompt_tokens_total{{engine_type="unified",is_streaming="true",model_name="m"}} {streaming}
sglang:prompt_tokens_total{{engine_type="unified",is_streaming="false",model_name="m"}} {other}
# TYPE sglang:cached_tokens_total counter
sglang:cached_tokens_total{{cache_source="device",engine_type="unified",model_name="m"}} {cached}
"#
            )
        };
        e.apply(scrape(base, 0.0, &text(10000.0, 500.0, 5000.0)));
        // The is_streaming="false" child re-registers at 0 while its sibling
        // keeps growing, so the SUM still rises (10500 -> 11000). Tracking
        // the sum would compute 800/500 -> a fake 100% cache share.
        e.apply(scrape(base, 1.0, &text(11000.0, 0.0, 5800.0)));
        assert_eq!(e.derived[&key()].cached_token_share_window, None);
        // Recovers on the next clean interval: 300 cached of 1000 prompt.
        e.apply(scrape(base, 2.0, &text(12000.0, 0.0, 6100.0)));
        let share = e.derived[&key()].cached_token_share_window.unwrap();
        assert!((share - 0.3).abs() < 1e-9, "share was {share}");
    }

    #[test]
    fn a_single_seriesless_scrape_does_not_wipe_chart_history() {
        let base = Instant::now();
        let mut e = ep();
        for i in 0..5 {
            let v = 1000.0 + i as f64 * 100.0;
            e.apply(scrape(base, i as f64, &metrics_text(v, v, 10.0, 0.0)));
        }
        assert_eq!(e.history[&(key(), series_id::RUNNING)].len(), 5);

        // A successful scrape that curates no series at all (a reshaped
        // response, or a /v1/loads fallback keyed per DP rank) must not
        // destroy history the server never invalidated.
        e.apply(scrape(base, 5.0, "up 1\n"));
        assert_eq!(e.history[&(key(), series_id::RUNNING)].len(), 5);

        // History resumes on the same ring once the series returns.
        e.apply(scrape(base, 6.0, &metrics_text(1600.0, 1600.0, 10.0, 0.0)));
        assert_eq!(e.history[&(key(), series_id::RUNNING)].len(), 6);
    }

    #[test]
    fn history_is_evicted_after_a_sustained_absence() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 1000.0, 10.0, 0.0)));
        assert!(!e.history.is_empty());
        // Bounded memory still holds: a series gone for good is dropped once
        // the grace period lapses.
        for i in 1..=(SERIES_EVICT_AFTER_SCRAPES as u64 + 1) {
            e.apply(scrape(base, i as f64, "up 1\n"));
        }
        assert!(e.history.is_empty(), "{} rings survived", e.history.len());
    }

    #[test]
    fn server_estimated_throughput_never_sets_a_session_peak() {
        let base = Instant::now();
        let mut e = ep();
        e.served_models = vec![ServedModel {
            id: "m".into(),
            root: None,
            max_model_len: None,
        }];
        let loads: crate::metrics::loads::LoadsResponse = serde_json::from_str(
            r#"{"loads":[{"dp_rank":0,"num_running_reqs":1,"gen_throughput":500.0}]}"#,
        )
        .unwrap();
        e.apply(ScrapeOutcome {
            at: base,
            wall: SystemTime::now(),
            duration: Duration::from_millis(5),
            result: Ok(ScrapePayload {
                loads: Some(loads),
                ..Default::default()
            }),
        });
        // The gauge is displayed (flagged `~`) but is not reset-detectable,
        // so it must not become a peak that outlives the process.
        assert_eq!(e.derived[&key()].generation_tps, Some(500.0));
        assert!(e.derived[&key()].generation_tps_is_server_est);
        assert_eq!(e.peak_generation_tps, None);
    }

    #[test]
    fn failure_preserves_last_good_snapshot() {
        let base = Instant::now();
        let mut e = ep();
        e.apply(scrape(base, 0.0, &metrics_text(1000.0, 500.0, 10.0, 0.0)));
        e.apply(ScrapeOutcome {
            at: base + Duration::from_secs(1),
            wall: SystemTime::now(),
            duration: Duration::from_millis(5),
            result: Err("connection refused".into()),
        });
        assert!(matches!(e.status, ConnStatus::Failing { .. }));
        // Snapshot survives.
        assert!(e.curated.is_some());
        assert_eq!(
            e.curated.as_ref().unwrap().series[&key()].running,
            Some(2.0)
        );
        assert_eq!(e.total_failures, 1);
    }

    #[test]
    fn consecutive_failures_counted() {
        let base = Instant::now();
        let mut e = ep();
        for i in 0..3 {
            e.apply(ScrapeOutcome {
                at: base + Duration::from_secs(i),
                wall: SystemTime::now(),
                duration: Duration::ZERO,
                result: Err("timeout".into()),
            });
        }
        match &e.status {
            ConnStatus::Failing { consecutive, .. } => assert_eq!(*consecutive, 3),
            s => panic!("unexpected status {s:?}"),
        }
    }

    #[test]
    fn freshness_transitions() {
        let base = Instant::now();
        let mut e = ep();
        let interval = Duration::from_secs(1);
        assert_eq!(e.freshness(base, interval), Freshness::Never);
        e.apply(scrape(base, 0.0, &metrics_text(1.0, 1.0, 1.0, 0.0)));
        assert_eq!(
            e.freshness(base + Duration::from_secs(2), interval),
            Freshness::Fresh
        );
        assert_eq!(
            e.freshness(base + Duration::from_secs(30), interval),
            Freshness::Stale
        );
    }

    #[test]
    fn attempt_state_distinguishes_in_flight_from_last_success() {
        let base = Instant::now();
        let mut e = ep();
        e.mark_attempt_started(base);
        assert_eq!(e.last_attempt_at, Some(base));
        assert_eq!(e.scraping_since, Some(base));
        assert_eq!(e.last_ok_at, None);

        e.apply(scrape(base, 2.0, &metrics_text(1.0, 1.0, 1.0, 0.0)));
        assert_eq!(e.scraping_since, None);
        assert_eq!(e.last_attempt_at, Some(base));
        assert_eq!(e.last_ok_at, Some(base + Duration::from_secs(2)));
    }

    #[test]
    fn history_accumulates() {
        let base = Instant::now();
        let mut e = ep();
        for i in 0..5 {
            let v = 1000.0 + i as f64 * 100.0;
            e.apply(scrape(base, i as f64, &metrics_text(v, v, 10.0, 0.0)));
        }
        let running = &e.history[&(key(), series_id::RUNNING)];
        assert_eq!(running.len(), 5);
        // Rates only exist from the second scrape on.
        let tps = &e.history[&(key(), series_id::PROMPT_TPS)];
        assert_eq!(tps.len(), 4);
        assert_eq!(tps.latest().unwrap().value, 100.0);
    }

    #[test]
    fn kv_aggregation_weighted_vs_unweighted() {
        // Both capacities known: weighted.
        let agg = aggregate_kv(&[(0.9, Some(1000.0)), (0.1, Some(9000.0))]).unwrap();
        assert!(agg.is_weighted());
        assert!((agg.value() - 0.18).abs() < 1e-9);
        // Any capacity missing: unweighted mean, flagged.
        let agg = aggregate_kv(&[(0.9, Some(1000.0)), (0.1, None)]).unwrap();
        assert!(!agg.is_weighted());
        assert!((agg.value() - 0.5).abs() < 1e-9);
        assert_eq!(aggregate_kv(&[]), None);
    }

    #[test]
    fn zero_vs_unavailable_distinction() {
        let base = Instant::now();
        let mut e = ep();
        // Endpoint exposes running but nothing else.
        let text =
            "# TYPE sglang:num_running_reqs gauge\nsglang:num_running_reqs{model_name=\"m\"} 0.0\n";
        e.apply(scrape(base, 0.0, text));
        let k = key();
        let c = &e.curated.as_ref().unwrap().series[&k];
        assert_eq!(c.running, Some(0.0)); // real zero
        assert_eq!(c.waiting, None); // unavailable, distinct from zero
        assert_eq!(c.kv_cache_usage, None);
    }
}

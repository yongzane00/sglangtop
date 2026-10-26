//! Per-endpoint polling tasks.
//!
//! Each configured endpoint gets independent tokio tasks that:
//! - start non-overlapping `/metrics` requests on an anchored cadence;
//! - skip cadence slots missed by a slow request instead of catching up;
//! - use fixed quarter-second fleet phase offsets instead of random jitter;
//! - probe `/ping`, `/server_info`, and `/v1/models` independently
//!   (never `/health`: it runs a real generation on many builds);
//! - parses the exposition text off the UI path;
//! - reports a [`ScrapeOutcome`] over one shared bounded channel;
//! - never blocks or crashes any other endpoint's collection.
//!
//! Auth material resolved from the environment lives only inside the
//! reqwest client's default headers; it is never sent over the state
//! channel, stored, or formatted into errors.

use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{mpsc, watch};

use crate::config::{Config, EndpointConfig};
use crate::event::AppEvent;
use crate::metrics::parse::parse_text;
use crate::state::{ScrapeOutcome, ScrapePayload, ServedModel, ServerInfo};

/// Cap on a `/metrics` response body; beyond this the scrape is an error
/// (runaway label cardinality would otherwise eat the monitor's memory).
pub const MAX_METRICS_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Cap on `/server_info` and `/v1/models` bodies. `/server_info` embeds one
/// `internal_states` copy of the full ServerArgs (~30 KiB) per DP rank, so
/// this must accommodate large-DP deployments while still catching a URL
/// that points at the wrong service.
pub const MAX_METADATA_BODY_BYTES: usize = 1024 * 1024;

/// Fetch `/server_info` and `/v1/models` every N cycles.
const METADATA_EVERY_CYCLES: u64 = 30;

/// Fleet-wide deterministic staggering. Endpoints after the fourth reuse the
/// four quarter-second slots rather than introducing per-cycle randomness.
const PHASE_STEP: Duration = Duration::from_millis(250);
const PHASE_SLOTS: usize = 4;

fn phase_offset(endpoint_index: usize) -> Duration {
    PHASE_STEP * (endpoint_index % PHASE_SLOTS) as u32
}

/// Return the first cadence slot strictly after `now`. Advancing from the
/// original anchor keeps starts phase-aligned; looping skips every slot that a
/// slow request occupied instead of queueing catch-up work.
fn next_slot_strictly_after(anchor: Instant, cadence: Duration, now: Instant) -> Instant {
    let mut next = anchor;
    while next <= now {
        next += cadence;
    }
    next
}

/// Handles for steering running collectors.
pub struct CollectorControl {
    /// Broadcast the (runtime-adjustable) refresh interval.
    pub interval_tx: watch::Sender<Duration>,
    /// Bump to force an immediate refresh of all endpoints.
    pub force_tx: watch::Sender<u64>,
}

impl CollectorControl {
    pub fn force_refresh(&self) {
        self.force_tx.send_modify(|n| *n = n.wrapping_add(1));
    }

    pub fn set_interval(&self, interval: Duration) {
        // send_if_modified: an unchanged value (e.g. key auto-repeat at the
        // clamp floor) must not wake every collector.
        self.interval_tx.send_if_modified(|current| {
            if *current == interval {
                false
            } else {
                *current = interval;
                true
            }
        });
    }
}

/// Spawn one collector task per endpoint. Returns control handles.
/// `events` is the single bounded channel into the main loop.
pub fn spawn_all(config: &Config, events: mpsc::Sender<AppEvent>) -> CollectorControl {
    let (interval_tx, _) = watch::channel(config.refresh_interval);
    let (force_tx, _) = watch::channel(0u64);

    // Build every client BEFORE anchoring the fleet phases. Constructing a
    // rustls client loads the platform root store, which on a cold process
    // can take longer than the offsets themselves — anchoring first would
    // leave the early slots already in the past, and the whole fleet would
    // fire its first round simultaneously, exactly the thundering herd the
    // stagger exists to prevent.
    let clients: Vec<(Option<reqwest::Client>, Option<String>)> = config
        .endpoints
        .iter()
        .map(|endpoint| match build_client(endpoint) {
            Ok(client) => (Some(client), None),
            Err(e) => (None, Some(e)),
        })
        .collect();

    let fleet_anchor = Instant::now();
    for ((idx, endpoint), (client, auth_error)) in config.endpoints.iter().enumerate().zip(clients)
    {
        let task = CollectorTask {
            idx,
            endpoint: endpoint.clone(),
            events: events.clone(),
            interval_rx: interval_tx.subscribe(),
            force_rx: force_tx.subscribe(),
            client: client.clone(),
            auth_error,
            first_start: fleet_anchor + phase_offset(idx),
        };
        let optional = OptionalTask {
            idx,
            endpoint: endpoint.clone(),
            events: events.clone(),
            client,
            cadence: config.refresh_interval,
            first_start: fleet_anchor + phase_offset(idx) + Duration::from_millis(125),
            cycle: 0,
        };
        tokio::spawn(task.run());
        tokio::spawn(optional.run());
    }

    CollectorControl {
        interval_tx,
        force_tx,
    }
}

struct CollectorTask {
    idx: usize,
    endpoint: EndpointConfig,
    events: mpsc::Sender<AppEvent>,
    interval_rx: watch::Receiver<Duration>,
    force_rx: watch::Receiver<u64>,
    /// Client is None when auth env resolution failed; `auth_error` says why.
    client: Option<reqwest::Client>,
    auth_error: Option<String>,
    first_start: Instant,
}

impl CollectorTask {
    async fn run(mut self) {
        let mut next_start = self.first_start;
        loop {
            if !self.wait_for_start(&mut next_start).await {
                return;
            }
            let started = Instant::now();
            if self
                .events
                .send(AppEvent::MetricsStarted {
                    endpoint: self.idx,
                    at: started,
                })
                .await
                .is_err()
            {
                return;
            }
            let outcome = self.poll_once().await;
            if self
                .events
                .send(AppEvent::Scrape {
                    endpoint: self.idx,
                    outcome,
                })
                .await
                .is_err()
            {
                return; // main loop is gone; shut down quietly
            }
            let cadence = *self.interval_rx.borrow_and_update();
            next_start = next_slot_strictly_after(next_start, cadence, Instant::now());
        }
    }

    async fn poll_once(&mut self) -> ScrapeOutcome {
        let Some(client) = self.client.clone() else {
            // Auth env vars missing: report a static, clear error. No HTTP.
            return ScrapeOutcome {
                at: Instant::now(),
                wall: SystemTime::now(),
                duration: Duration::ZERO,
                result: Err(self
                    .auth_error
                    .clone()
                    .unwrap_or_else(|| "endpoint disabled".into())),
            };
        };

        let started = Instant::now();
        let metrics = fetch_metrics(&client, &self.endpoint).await;

        let mut payload = ScrapePayload {
            metrics: None,
            healthy: None,
            server_info: None,
            models: None,
            loads: None,
        };

        let result = match metrics {
            Ok(text) => {
                payload.metrics = Some(parse_text(&text));
                Ok(payload)
            }
            // /metrics 404s on servers launched without --enable-metrics.
            // /v1/loads is served unconditionally, so fall back to its
            // gauges rather than going dark. Never both sources at once.
            Err(e) if e.status == Some(404) => match fetch_loads(&client, &self.endpoint).await {
                Ok(loads) => {
                    payload.loads = Some(loads);
                    Ok(payload)
                }
                Err(loads_err) => Err(format!("{}; /v1/loads fallback: {loads_err}", e.message)),
            },
            Err(e) => Err(e.message),
        };
        let duration = started.elapsed();

        ScrapeOutcome {
            at: Instant::now(),
            wall: SystemTime::now(),
            duration,
            result,
        }
    }

    /// Wait for the next anchored slot. Force-refresh may start early while
    /// idle; interval changes reset the fleet's fixed phase from change time.
    async fn wait_for_start(&mut self, next_start: &mut Instant) -> bool {
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until((*next_start).into()) => return true,
                res = self.force_rx.changed() => {
                    if res.is_ok() {
                        self.force_rx.borrow_and_update();
                        return true; // user asked for an immediate refresh
                    }
                    tokio::time::sleep_until((*next_start).into()).await;
                    return true;
                }
                res = self.interval_rx.changed() => {
                    if res.is_err() {
                        tokio::time::sleep_until((*next_start).into()).await;
                        return true;
                    }
                    self.interval_rx.borrow_and_update();
                    *next_start = Instant::now() + phase_offset(self.idx);
                }
            }
        }
    }
}

/// Optional probes have their own sequential schedule and client handle, so
/// none of them can hold up `/metrics` publication. They may overlap metrics,
/// but optional cycles cannot overlap one another or queue missed ticks.
struct OptionalTask {
    idx: usize,
    endpoint: EndpointConfig,
    events: mpsc::Sender<AppEvent>,
    client: Option<reqwest::Client>,
    cadence: Duration,
    first_start: Instant,
    cycle: u64,
}

impl OptionalTask {
    async fn run(mut self) {
        let Some(client) = self.client.take() else {
            return;
        };
        let mut next_start = self.first_start;
        loop {
            tokio::time::sleep_until(next_start.into()).await;
            let want_metadata = self.cycle.is_multiple_of(METADATA_EVERY_CYCLES);
            let (healthy, server_info, models) = if want_metadata {
                tokio::join!(
                    fetch_liveness(&client, &self.endpoint),
                    fetch_server_info(&client, &self.endpoint),
                    fetch_models(&client, &self.endpoint),
                )
            } else {
                (fetch_liveness(&client, &self.endpoint).await, None, None)
            };
            if self
                .events
                .send(AppEvent::OptionalUpdate {
                    endpoint: self.idx,
                    healthy,
                    server_info,
                    models,
                })
                .await
                .is_err()
            {
                return;
            }
            self.cycle = self.cycle.wrapping_add(1);
            next_start = next_slot_strictly_after(next_start, self.cadence, Instant::now());
        }
    }
}

/// Build a reqwest client with resolved auth headers. The only place secret
/// values exist outside the environment.
fn build_client(endpoint: &EndpointConfig) -> Result<reqwest::Client, String> {
    use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};

    let auth = endpoint.resolve_auth(|k| std::env::var(k).ok())?;

    let mut headers = HeaderMap::new();
    if let Some(token) = auth.bearer {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "bearer token contains invalid header characters".to_string())?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    for (name, value) in auth.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
        let mut value = HeaderValue::from_str(&value)
            .map_err(|_| format!("header {name:?} value contains invalid characters"))?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(10))
        // Metrics endpoints don't redirect; refusing redirects also removes
        // any chance of auth headers leaking to another origin.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("sglangtop/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

fn endpoint_url(endpoint: &EndpointConfig, path: &str) -> String {
    let base = endpoint.url.as_str();
    if base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// A `/metrics` failure with the HTTP status preserved, so the caller can
/// distinguish "endpoint absent" (404 → /v1/loads fallback) from transport
/// or server errors. Messages stay short and header-free.
struct FetchError {
    status: Option<u16>,
    message: String,
}

/// Errors are reduced to short strings before leaving the collector; they
/// contain reqwest's message (URL host/port at most) and never headers.
async fn fetch_metrics(
    client: &reqwest::Client,
    endpoint: &EndpointConfig,
) -> Result<String, FetchError> {
    let url = endpoint_url(endpoint, "metrics");
    let resp = client.get(&url).send().await.map_err(|e| FetchError {
        status: None,
        message: http_error("/metrics", &e),
    })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(FetchError {
            status: Some(status.as_u16()),
            message: format!("/metrics returned HTTP {}", status.as_u16()),
        });
    }
    read_capped(resp, "/metrics", MAX_METRICS_BODY_BYTES)
        .await
        .map_err(|message| FetchError {
            status: None,
            message,
        })
}

/// Fetch the always-on `/v1/loads` snapshot (the metrics-disabled fallback).
async fn fetch_loads(
    client: &reqwest::Client,
    endpoint: &EndpointConfig,
) -> Result<crate::metrics::loads::LoadsResponse, String> {
    let url = endpoint_url(endpoint, "v1/loads");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| http_error("/v1/loads", &e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("/v1/loads returned HTTP {}", status.as_u16()));
    }
    let body = read_capped(resp, "/v1/loads", MAX_METADATA_BODY_BYTES).await?;
    serde_json::from_str(&body).map_err(|_| "/v1/loads body is not the expected JSON".to_string())
}

/// Read a body while enforcing a byte cap — EVERY response body goes through
/// this; `Response::json()`/`text()` would buffer without limit.
async fn read_capped(resp: reqwest::Response, what: &str, cap: usize) -> Result<String, String> {
    if let Some(len) = resp.content_length()
        && len as usize > cap
    {
        return Err(format!("{what} body is {len} bytes (limit {cap})"));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut resp = resp;
    while let Some(chunk) = resp.chunk().await.map_err(|e| http_error(what, &e))? {
        if buf.len() + chunk.len() > cap {
            return Err(format!("{what} body exceeds {cap} bytes"));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| format!("{what} body is not valid UTF-8"))
}

/// Liveness probe. Deliberately `/ping`, NOT `/health`.
///
/// In SGLang `/health` and `/health_generate` share one handler; `/health`
/// short-circuits to 200 only while `SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION`
/// is unset. With that variable set — and it is, on real deployments — every
/// `/health` call runs a real one-token generation (`input_ids=[0]`,
/// `max_new_tokens=1`). Polling it once per refresh cycle made the monitor
/// itself the workload: measured on a live 0.5.19 server, each call cost
/// ~1.0 s and exactly +1 `realtime_tokens_total{prefill_compute}` and +1
/// `{decode}`, so an idle server reported a permanent ~1 token/s of
/// sglangtop's own traffic. `log_metrics=False` on that request hides it from
/// the tokenizer counters, so it corrupts the scheduler-side rates only —
/// the ones this tool charts.
///
/// `/ping` is an unconditional `Response(status_code=200)` upstream: no
/// generation, no scheduler work, ~0.1 s. It is the only liveness endpoint
/// safe to call at cadence, which is what keeps sglangtop read-only.
///
/// Status semantics: 2xx = healthy, other statuses = unhealthy, but a server
/// that simply doesn't HAVE the endpoint (404/405/501) or that we cannot ask
/// (request error) is UNKNOWN — its absence must display as N/A, not as a red
/// health flag.
async fn fetch_liveness(client: &reqwest::Client, endpoint: &EndpointConfig) -> Option<bool> {
    let url = endpoint_url(endpoint, "ping");
    match client.get(&url).send().await {
        Ok(resp) => {
            let status = resp.status();
            if status.is_success() {
                Some(true)
            } else if matches!(status.as_u16(), 404 | 405 | 501) {
                None
            } else {
                Some(false)
            }
        }
        Err(_) => None,
    }
}

/// Fetch `/server_info` (ServerArgs + version + live scheduler facts).
/// SGLang deprecated `/get_server_info` in favor of `/server_info`; older
/// builds only have the former, so a 404/405 on the new path falls back to
/// the alias — one extra request per metadata cycle at most.
async fn fetch_server_info(
    client: &reqwest::Client,
    endpoint: &EndpointConfig,
) -> Option<ServerInfo> {
    for path in ["server_info", "get_server_info"] {
        let url = endpoint_url(endpoint, path);
        let Ok(resp) = client.get(&url).send().await else {
            return None; // transport error: the alias would fail identically
        };
        let status = resp.status();
        if matches!(status.as_u16(), 404 | 405) {
            continue; // path not on this build; try the alias
        }
        if !status.is_success() {
            return None;
        }
        let body = read_capped(resp, "/server_info", MAX_METADATA_BODY_BYTES)
            .await
            .ok()?;
        return serde_json::from_str::<ServerInfo>(&body).ok();
    }
    None
}

async fn fetch_models(
    client: &reqwest::Client,
    endpoint: &EndpointConfig,
) -> Option<Vec<ServedModel>> {
    #[derive(serde::Deserialize)]
    struct Models {
        data: Vec<ModelEntry>,
    }
    // `root` is the underlying repo id or local path; `max_model_len` is the
    // context window, which SGLang also exports as `sglang:context_len`.
    #[derive(serde::Deserialize)]
    struct ModelEntry {
        id: String,
        #[serde(default)]
        root: Option<String>,
        #[serde(default)]
        max_model_len: Option<u64>,
    }
    let url = endpoint_url(endpoint, "v1/models");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = read_capped(resp, "/v1/models", MAX_METADATA_BODY_BYTES)
        .await
        .ok()?;
    serde_json::from_str::<Models>(&body).ok().map(|m| {
        m.data
            .into_iter()
            .map(|e| ServedModel {
                id: e.id,
                root: e.root,
                max_model_len: e.max_model_len,
            })
            .collect()
    })
}

/// reqwest errors include the URL; keep host/path but make sure no query or
/// userinfo can appear by using `without_url` and appending our own path tag.
fn http_error(what: &str, e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connection failed"
    } else if e.is_body() || e.is_decode() {
        "body error"
    } else {
        "request failed"
    };
    match source_chain_root(e) {
        Some(detail) => format!("{what}: {kind}: {detail}"),
        None => format!("{what}: {kind}"),
    }
}

/// The innermost error source is the useful part ("Connection refused");
/// the outer layers repeat the URL, which we deliberately avoid echoing so
/// no query string or userinfo can leak into displayed errors.
fn source_chain_root(e: &reqwest::Error) -> Option<String> {
    let mut cur: &(dyn std::error::Error + 'static) = e;
    let mut last = None;
    while let Some(next) = cur.source() {
        last = Some(next.to_string());
        cur = next;
    }
    last
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use url::Url;

    fn ep(url: &str) -> EndpointConfig {
        EndpointConfig {
            name: "t".into(),
            url: Url::parse(url).unwrap(),
            bearer_token_env: None,
            header_env: BTreeMap::new(),
            max_running: None,
            log_file: None,
        }
    }

    #[test]
    fn url_joining_handles_trailing_slash_and_base_paths() {
        assert_eq!(
            endpoint_url(&ep("http://h:8000"), "metrics"),
            "http://h:8000/metrics"
        );
        assert_eq!(
            endpoint_url(&ep("http://h:8000/"), "metrics"),
            "http://h:8000/metrics"
        );
        assert_eq!(
            endpoint_url(&ep("http://h:8000/proxy/sglang"), "v1/models"),
            "http://h:8000/proxy/sglang/v1/models"
        );
        assert_eq!(
            endpoint_url(&ep("http://h:30000"), "server_info"),
            "http://h:30000/server_info"
        );
    }

    #[test]
    fn fleet_phase_offsets_repeat_in_four_quarter_second_slots() {
        let expected_ms = [0, 250, 500, 750, 0, 250, 500, 750];
        for (idx, expected) in expected_ms.into_iter().enumerate() {
            assert_eq!(phase_offset(idx), Duration::from_millis(expected));
        }
    }

    #[test]
    fn anchored_scheduler_skips_every_slot_missed_by_a_slow_scrape() {
        let anchor = Instant::now();
        let cadence = Duration::from_secs(1);
        assert_eq!(
            next_slot_strictly_after(anchor, cadence, anchor + Duration::from_millis(200)),
            anchor + Duration::from_secs(1)
        );
        assert_eq!(
            next_slot_strictly_after(anchor, cadence, anchor + Duration::from_millis(2_300)),
            anchor + Duration::from_secs(3)
        );
        assert_eq!(
            next_slot_strictly_after(anchor, cadence, anchor + Duration::from_secs(3)),
            anchor + Duration::from_secs(4)
        );
    }

    #[test]
    fn missing_auth_env_produces_client_error_not_panic() {
        let endpoint = EndpointConfig {
            name: "s".into(),
            url: Url::parse("https://h:1").unwrap(),
            bearer_token_env: Some("SGLANGTOP_TEST_UNSET_VAR_XYZ".into()),
            header_env: BTreeMap::new(),
            max_running: None,
            log_file: None,
        };
        let err = build_client(&endpoint).unwrap_err();
        assert!(err.contains("SGLANGTOP_TEST_UNSET_VAR_XYZ"));
    }
}

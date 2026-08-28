//! End-to-end pipeline tests: mock HTTP server → collector → parser →
//! normalization → state, including failure isolation and recovery.
//!
//! No test here touches a live network service; everything runs against
//! local httpmock servers or fixtures.

use std::time::Duration;

use clap::Parser;
use httpmock::prelude::*;
use tokio::sync::mpsc;

use sglangtop::cli::Cli;
use sglangtop::collector::spawn_all;
use sglangtop::config::{Config, load};
use sglangtop::event::AppEvent;
use sglangtop::metrics::normalize::{SeriesKey, curate, hist};
use sglangtop::metrics::parse::parse_text;
use sglangtop::state::{ConnStatus, EndpointState, Freshness};

/// Ground-truth `/metrics` capture from a real SGLang 0.5.19 server
/// (Qwen/Qwen3.8-27B-FP8, unified, spec decoding DFLASH, tp=1 dp=1).
const FIXTURE: &str = include_str!("fixtures/sglang_unified_single.txt");
const SERVER_INFO_FIXTURE: &str = include_str!("fixtures/sglang_server_info.json");
const MODEL: &str = "Qwen/Qwen3.8-27B-FP8";
const VERSION: &str = "0.5.19.dev99+g1cf2b8c54";

/// The `/server_info` capture from the same server (481 top-level fields)
/// must deserialize into our defensive subset.
#[test]
fn live_server_info_capture_deserializes() {
    let info: sglangtop::state::ServerInfo =
        serde_json::from_str(SERVER_INFO_FIXTURE).expect("real capture parses");
    assert_eq!(info.version.as_deref(), Some(VERSION));
    assert_eq!(info.max_running_requests, Some(48));
    assert_eq!(info.max_total_num_tokens, Some(131016));
    assert_eq!(info.speculative_algorithm.as_deref(), Some("DFLASH"));
    assert_eq!(info.enable_metrics, Some(true));
    assert_eq!(info.dp_size, Some(1));
    assert_eq!(info.pd_role(), None); // disaggregation_mode is the string "null"
}

fn config_for(urls: &[String]) -> Config {
    let mut args = vec![
        "sglangtop".to_string(),
        "--refresh-interval-ms".into(),
        "1000".into(),
    ];
    for (i, u) in urls.iter().enumerate() {
        args.push("--endpoint".into());
        args.push(format!("ep{i}={u}"));
    }
    load(&Cli::parse_from(args), |_| None).expect("valid test config")
}

fn state_for(config: &Config, idx: usize) -> EndpointState {
    EndpointState::new(
        config.endpoints[idx].name.clone(),
        config.endpoints[idx].display_url(),
        config.history_window,
        config.percentile_window,
    )
}

async fn next_scrape_for(
    rx: &mut mpsc::Receiver<AppEvent>,
    endpoint: usize,
) -> sglangtop::state::ScrapeOutcome {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for scrape event")
            .expect("event channel closed unexpectedly");
        match event {
            AppEvent::Scrape {
                endpoint: idx,
                outcome,
            } if idx == endpoint => return outcome,
            _ => {}
        }
    }
}

async fn next_metrics_started_for(
    rx: &mut mpsc::Receiver<AppEvent>,
    endpoint: usize,
) -> std::time::Instant {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("event channel closed unexpectedly");
            match event {
                AppEvent::MetricsStarted { endpoint: idx, at } if idx == endpoint => return at,
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for metrics-start event")
}

async fn apply_next_optional_for(
    rx: &mut mpsc::Receiver<AppEvent>,
    endpoint: usize,
    state: &mut EndpointState,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("event channel closed unexpectedly");
            match event {
                AppEvent::OptionalUpdate {
                    endpoint: idx,
                    healthy,
                    server_info,
                    models,
                } if idx == endpoint => {
                    state.apply_optional(healthy, server_info, models);
                    return;
                }
                AppEvent::Scrape {
                    endpoint: idx,
                    outcome,
                } if idx == endpoint => state.apply(outcome),
                _ => {}
            }
        }
    })
    .await
    .expect("timed out waiting for optional-probe event");
}

fn serve_fixture(server: &MockServer) {
    server.mock(|when, then| {
        when.method(GET).path("/metrics");
        then.status(200)
            .header("content-type", "text/plain; version=0.0.4")
            .body(FIXTURE);
    });
    server.mock(|when, then| {
        when.method(GET).path("/ping");
        then.status(200);
    });
    server.mock(|when, then| {
        when.method(GET).path("/server_info");
        // The real capture: hundreds of ServerArgs fields, all of which must
        // be ignored except our defensive subset.
        then.status(200)
            .header("content-type", "application/json")
            .body(SERVER_INFO_FIXTURE);
    });
    server.mock(|when, then| {
        when.method(GET).path("/v1/models");
        then.status(200)
            .header("content-type", "application/json")
            .body(format!(
                r#"{{"object":"list","data":[{{"id":"{MODEL}","object":"model"}}]}}"#
            ));
    });
}

#[tokio::test]
async fn full_pipeline_against_mock_server() {
    let server = MockServer::start_async().await;
    serve_fixture(&server);

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut state = state_for(&config, 0);
    let outcome = next_scrape_for(&mut rx, 0).await;
    state.apply(outcome);
    apply_next_optional_for(&mut rx, 0, &mut state).await;

    // Connection + optional endpoints.
    assert_eq!(state.status, ConnStatus::Connected);
    assert_eq!(state.healthy, Some(true));
    assert_eq!(state.server_version.as_deref(), Some(VERSION));
    let info = state.server_info.as_ref().expect("server_info probed");
    assert_eq!(info.max_running_requests, Some(48));
    assert_eq!(info.enable_metrics, Some(true));
    assert_eq!(info.pd_role(), None); // "null" string means unified
    assert_eq!(state.effective_max_running(None), Some(48));
    assert_eq!(state.served_models, vec![MODEL]);
    assert_eq!(
        state.freshness(std::time::Instant::now(), config.refresh_interval),
        Freshness::Fresh
    );

    // Curated values from the real SGLang 0.5.19 capture. The unified
    // engine_type normalizes out of the key, so scheduler and tokenizer
    // metrics land on ONE series.
    let key = SeriesKey {
        model: MODEL.into(),
        engine_type: None,
        dp_rank: None,
    };
    let curated = state.curated.as_ref().unwrap();
    assert_eq!(curated.series.len(), 1);
    let series = &curated.series[&key];
    assert_eq!(series.running, Some(0.0));
    assert_eq!(series.waiting, Some(0.0));
    assert_eq!(series.kv_cache_usage, Some(0.0));
    assert_eq!(series.kv_cache_size_tokens, Some(131_016.0));
    assert_eq!(series.kv_used_tokens, Some(0.0));
    // Tokenizer counters, summed over is_streaming children.
    assert_eq!(series.prompt_tokens_total(), Some(73_311.0));
    assert_eq!(series.generation_tokens_total(), Some(14_212.0));
    assert_eq!(series.requests_total(), Some(5.0));
    assert_eq!(series.cached_tokens_total(), Some(44_096.0));
    // Scheduler realtime counters (the live rate source).
    assert_eq!(series.realtime_tokens_by_mode["decode"], 14_220.0);
    assert_eq!(series.realtime_tokens_by_mode["prefill_compute"], 29_215.0);
    assert_eq!(series.realtime_tokens_by_mode["prefill_cache"], 44_096.0);
    // Spec decoding was active on the captured server.
    assert_eq!(series.spec_accept_length, Some(3.325));
    assert!(series.spec_accept_rate.unwrap() > 0.33);
    // Histograms: is_streaming children merged by bucket-wise sum.
    assert_eq!(series.histograms[hist::TTFT].count, 5.0);
    assert_eq!(series.histograms[hist::E2E_LATENCY].count, 5.0);
    assert_eq!(series.histograms[hist::INTER_TOKEN_LATENCY].count, 14_151.0);
    assert_eq!(series.histograms[hist::QUEUE_TIME].count, 5.0);
    assert_eq!(series.histograms[hist::STAGE_PREFILL_FORWARD].count, 5.0);
    assert!(series.histograms.contains_key(hist::PROMPT_TOKENS_PER_REQ));

    // Second scrape: rates become available (deltas are zero here).
    let outcome = next_scrape_for(&mut rx, 0).await;
    state.apply(outcome);
    let derived = &state.derived[&key];
    assert_eq!(derived.prompt_tps, Some(0.0));
    assert_eq!(derived.generation_tps, Some(0.0));
    assert!(!derived.generation_tps_is_server_est);
    assert_eq!(derived.request_rate, Some(0.0));
}

#[tokio::test]
async fn failing_endpoint_does_not_block_healthy_one() {
    let server = MockServer::start_async().await;
    serve_fixture(&server);
    // Port 9 (discard) on localhost: refused immediately on any sane host.
    let dead = "http://127.0.0.1:9".to_string();

    let config = config_for(&[server.base_url(), dead]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut healthy = state_for(&config, 0);
    let mut failing = state_for(&config, 1);

    healthy.apply(next_scrape_for(&mut rx, 0).await);
    failing.apply(next_scrape_for(&mut rx, 1).await);

    assert_eq!(healthy.status, ConnStatus::Connected);
    assert!(healthy.curated.is_some());
    match &failing.status {
        ConnStatus::Failing { error, .. } => {
            assert!(!error.is_empty());
            // Failure text must not leak any auth material (none set here,
            // but the invariant is that errors never carry header values).
            assert!(!error.to_lowercase().contains("authorization"));
        }
        s => panic!("expected failing endpoint, got {s:?}"),
    }
    // The healthy endpoint keeps scraping at its interval.
    healthy.apply(next_scrape_for(&mut rx, 0).await);
    assert_eq!(healthy.total_scrapes, 2);
    assert_eq!(healthy.total_failures, 0);
}

#[tokio::test]
async fn endpoint_recovers_after_coming_back() {
    let server = MockServer::start_async().await;
    let broken = server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(500);
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/ping");
            then.status(200);
        })
        .await;

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut state = state_for(&config, 0);
    state.apply(next_scrape_for(&mut rx, 0).await);
    assert!(matches!(state.status, ConnStatus::Failing { .. }));
    assert!(state.curated.is_none());

    // Server comes back.
    broken.delete_async().await;
    serve_fixture(&server);

    // Collector backs off after a failure, so allow a few cycles.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        state.apply(next_scrape_for(&mut rx, 0).await);
        if state.status == ConnStatus::Connected || std::time::Instant::now() > deadline {
            break;
        }
    }
    assert_eq!(state.status, ConnStatus::Connected);
    assert!(state.curated.is_some());
    assert_eq!(
        state.freshness(std::time::Instant::now(), config.refresh_interval),
        Freshness::Fresh
    );
}

#[tokio::test]
async fn missing_optional_endpoints_leave_metadata_unknown() {
    let server = MockServer::start_async().await;
    // Only /metrics exists; /ping /server_info /v1/models all 404.
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200).body(FIXTURE);
        })
        .await;

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut state = state_for(&config, 0);
    state.apply(next_scrape_for(&mut rx, 0).await);

    // Monitoring continues; the missing endpoints degrade to unknown/N/A.
    assert_eq!(state.status, ConnStatus::Connected);
    assert!(state.curated.is_some());
    assert_eq!(state.server_version, None);
    assert!(state.served_models.is_empty());
    // A server without /ping (404) is UNKNOWN, not unhealthy: the endpoint
    // is optional and its absence must show as N/A.
    assert_eq!(state.healthy, None);
}

#[tokio::test]
async fn slow_metrics_scrape_skips_missed_tick_and_stays_anchored() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200)
                .delay(Duration::from_millis(1_500))
                .body(FIXTURE);
        })
        .await;

    let args = [
        "sglangtop".to_string(),
        "--refresh-interval-ms".into(),
        "1000".into(),
        "--endpoint".into(),
        format!("ep0={}", server.base_url()),
    ];
    let config = load(&Cli::parse_from(args), |_| None).unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let first = next_metrics_started_for(&mut rx, 0).await;
    let second = next_metrics_started_for(&mut rx, 0).await;
    let gap = second.duration_since(first);
    assert!(gap >= Duration::from_millis(1_800), "gap was {gap:?}");
    assert!(gap <= Duration::from_millis(2_250), "gap was {gap:?}");
}

#[tokio::test]
async fn slow_optional_probe_cannot_delay_metrics_publication() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200).body(FIXTURE);
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/ping");
            then.status(200).delay(Duration::from_secs(2));
        })
        .await;

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);
    let _started = next_metrics_started_for(&mut rx, 0).await;
    let outcome = tokio::time::timeout(Duration::from_millis(500), next_scrape_for(&mut rx, 0))
        .await
        .expect("slow /ping delayed /metrics publication");
    assert!(outcome.result.is_ok(), "{outcome:?}");
}

#[tokio::test]
async fn first_metrics_attempts_use_fixed_quarter_second_fleet_phases() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(200).body(FIXTURE);
        })
        .await;
    let urls = vec![server.base_url(); 4];
    let config = config_for(&urls);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let starts = tokio::time::timeout(Duration::from_secs(2), async {
        let mut starts = [None; 4];
        while starts.iter().any(Option::is_none) {
            if let Some(AppEvent::MetricsStarted { endpoint, at }) = rx.recv().await
                && endpoint < starts.len()
                && starts[endpoint].is_none()
            {
                starts[endpoint] = Some(at);
            }
        }
        starts.map(Option::unwrap)
    })
    .await
    .expect("fleet did not start all first attempts");

    for (idx, expected_ms) in [0_u64, 250, 500, 750].into_iter().enumerate() {
        let actual = starts[idx].duration_since(starts[0]);
        let expected = Duration::from_millis(expected_ms);
        assert!(
            actual.abs_diff(expected) <= Duration::from_millis(200),
            "endpoint {idx}: expected {expected:?}, got {actual:?}"
        );
    }
}

#[test]
fn live_fixture_parses_cleanly_and_completely() {
    let scrape = parse_text(FIXTURE);
    assert!(
        scrape.issues.is_empty(),
        "real SGLang output must parse without issues: {:?}",
        scrape.issues
    );
    // The capture contains 501 samples across 71 families.
    assert_eq!(scrape.total_samples(), 501);
    assert_eq!(scrape.families.len(), 71);
    // Colon-prefixed names and the `_histogram`-suffixed histogram family
    // both keep their declared types.
    let prompt_hist = scrape.family("sglang:prompt_tokens_histogram").unwrap();
    assert_eq!(
        prompt_hist.kind,
        sglangtop::metrics::model::MetricType::Histogram
    );
    let retracted = scrape.family("sglang:num_retracted_reqs").unwrap();
    assert_eq!(retracted.kind, sglangtop::metrics::model::MetricType::Gauge);
    // A live NaN gauge (fwd_occupancy while idle) survives parsing.
    assert!(scrape.family("sglang:fwd_occupancy").is_some());
}

#[test]
fn unknown_backend_specific_metrics_survive_to_raw_view() {
    let text = "\
# TYPE habana_specific_thing gauge
habana_specific_thing{device=\"hpu0\"} 42.0
sglang:future_metric_without_type 7.0
";
    let scrape = parse_text(text);
    assert_eq!(scrape.total_samples(), 2);
    assert!(scrape.family("habana_specific_thing").is_some());
    assert!(scrape.family("sglang:future_metric_without_type").is_some());
    // They contribute no curated series but are not lost.
    let curated = curate(&scrape);
    assert!(curated.series.is_empty());
}

/// sglangtop must NEVER touch `/health` or `/health_generate`. Upstream they
/// share one handler, and with `SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION` set
/// `/health` runs a real one-token generation: measured on a live 0.5.19
/// server at ~1.0 s and exactly +1 `realtime_tokens_total{prefill_compute}`
/// and +1 `{decode}` per call. Polling it at cadence made an idle server
/// report a permanent ~1 token/s of the monitor's own traffic. `/ping` is an
/// unconditional 200 upstream and is the only safe liveness probe.
#[tokio::test]
async fn liveness_probe_never_calls_generating_health_endpoints() {
    let server = MockServer::start_async().await;
    serve_fixture(&server);
    // Register the dangerous endpoints so any request to them is recorded.
    let health = server
        .mock_async(|when, then| {
            when.method(GET).path("/health");
            then.status(200);
        })
        .await;
    let health_generate = server
        .mock_async(|when, then| {
            when.method(GET).path("/health_generate");
            then.status(200);
        })
        .await;
    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    // Let several probe cycles run.
    let mut state = state_for(&config, 0);
    for _ in 0..2 {
        state.apply(next_scrape_for(&mut rx, 0).await);
        apply_next_optional_for(&mut rx, 0, &mut state).await;
    }

    assert_eq!(
        health.calls(),
        0,
        "/health must never be probed: it runs a real generation on servers \
         with SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION set"
    );
    assert_eq!(
        health_generate.calls(),
        0,
        "/health_generate always runs a real generation"
    );
    // Healthy can only have been established by the `/ping` mock that
    // `serve_fixture` registers, so liveness is genuinely probed elsewhere.
    assert_eq!(state.healthy, Some(true));
}

/// A server without `--enable-metrics` 404s on `/metrics` but serves
/// `/v1/loads` unconditionally: the collector falls back to its gauges,
/// counter-derived values stay honestly absent, and the endpoint is marked
/// with the `loads_only` capability.
#[tokio::test]
async fn metrics_404_falls_back_to_loads_only() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/metrics");
            then.status(404);
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/v1/loads");
            then.status(200)
                .header("content-type", "application/json")
                .body(include_str!("fixtures/sglang_v1_loads.json"));
        })
        .await;

    let config = config_for(&[server.base_url()]);
    let (tx, mut rx) = mpsc::channel(64);
    let _control = spawn_all(&config, tx);

    let mut state = state_for(&config, 0);
    // Give the state a model name up front (normally arrives via the
    // optional probe) so the series key is stable from the first scrape.
    state.apply_optional(None, None, Some(vec![MODEL.to_string()]));
    state.apply(next_scrape_for(&mut rx, 0).await);

    assert_eq!(state.status, ConnStatus::Connected);
    assert_eq!(
        state.freshness(std::time::Instant::now(), config.refresh_interval),
        Freshness::Fresh
    );
    let curated = state.curated.as_ref().unwrap();
    assert!(curated.has(sglangtop::metrics::loads::LOADS_ONLY));
    let key = SeriesKey {
        model: MODEL.into(),
        engine_type: None,
        dp_rank: None,
    };
    let series = &curated.series[&key];
    assert_eq!(series.running, Some(1.0));
    assert_eq!(series.kv_cache_usage, Some(0.1828));
    assert_eq!(series.kv_cache_size_tokens, Some(131_016.0));
    // Counter-derived values are honestly absent, not zero.
    assert!(series.histograms.is_empty());
    assert_eq!(series.prompt_tokens_total(), None);
    let derived = &state.derived[&key];
    assert_eq!(derived.prompt_tps, None);
    assert_eq!(derived.request_rate, None);
    // Decode throughput passes through the server's own gauge, flagged.
    assert_eq!(derived.generation_tps, Some(0.0));
    assert!(derived.generation_tps_is_server_est);
}

/// PD-disaggregated deployments are separate prefill/decode server processes;
/// each endpoint curates with its engine_type in the series key and its
/// role's queue gauges present.
#[test]
fn pd_disaggregated_fixtures_curate_with_roles() {
    let prefill = curate(&parse_text(include_str!("fixtures/sglang_pd_prefill.txt")));
    let pk = SeriesKey {
        model: "example/pd-model".into(),
        engine_type: Some("prefill".into()),
        dp_rank: None,
    };
    let ps = &prefill.series[&pk];
    assert_eq!(ps.running, Some(3.0));
    assert_eq!(ps.pd_bootstrap_queue, Some(2.0));
    assert_eq!(ps.pd_inflight_queue, Some(4.0));
    assert!(prefill.has("pd_queues"));
    assert_eq!(pk.display(), "example/pd-model [prefill]");

    let decode = curate(&parse_text(include_str!("fixtures/sglang_pd_decode.txt")));
    let dk = SeriesKey {
        model: "example/pd-model".into(),
        engine_type: Some("decode".into()),
        dp_rank: None,
    };
    let ds = &decode.series[&dk];
    assert_eq!(ds.pd_prealloc_queue, Some(1.0));
    assert_eq!(ds.pd_transfer_queue, Some(2.0));
    // kv_transfer_latency_ms curates in seconds.
    let kv = &ds.histograms[hist::KV_TRANSFER_LATENCY];
    assert!(kv.buckets.iter().any(|(le, _)| *le == 0.025));
    assert!((kv.sum - 0.19).abs() < 1e-12);
}

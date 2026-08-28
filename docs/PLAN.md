# sglangtop — Implementation Plan

Status: living document. Milestones are checked off as they are completed and verified.

## What is being built

A single-binary Linux TUI (`sglangtop`) that polls one or more SGLang servers
over read-only HTTP (`/metrics`, plus `/ping`, `/server_info`, `/v1/models`,
and the `/v1/loads` fallback when metrics are disabled) and renders fleet +
per-endpoint serving telemetry in a dense, DGXTOP-style terminal UI. No
proxying, no inference traffic, no accelerator APIs, no per-user/conversation
attribution. Neither `/health` nor `/health_generate` is ever probed: upstream
they share one handler, and with `SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION` set
`/health` runs a real one-token generation, so polling it at cadence would make
the monitor a permanent ~1 token/s of load on an idle server. Liveness comes
from `/ping`, an unconditional 200.
Per-request visibility is deferred in v1 (key decision 14).

## Architecture (message passing, no shared mutable state)

```
per-endpoint collector tasks (tokio)         crossterm EventStream
  anchored cadence, missed slots skipped            |
  GET /metrics (404 -> /v1/loads fallback)          v
  (+ping/server_info/models)    --bounded mpsc--> main select! loop (owns AppState)
  parse -> RawScrape                         |  reduce: normalize -> rates ->
                                             |  histogram windows -> ring history
                                             |--bounded channel--> SQLite recorder
                                             |                     (dedicated thread)
                                             v
                                       Ratatui render (tick-driven)
```

- Collectors never touch shared state; they send `AppEvent::Scrape` messages.
- The main loop is the single owner of `AppState`; rendering borrows it.
- The optional recorder runs on its own OS thread; a slow/failed DB drops
  batches (with a counter) instead of blocking collection.

## Module map

```
src/
├── main.rs          entry; terminal guard (panic-safe restore); runtime
├── cli.rs           clap arg definitions (NAME=URL[@CAP] endpoint grammar)
├── config.rs        TOML schema, precedence merge, validation
├── app.rs           select! loop, event dispatch, key handling, view routing
├── event.rs         AppEvent / input mapping
├── collector/       reqwest client (rustls), per-endpoint poll tasks
├── metrics/         parse.rs (own Prometheus text parser), model.rs (types),
│                    normalize.rs (sglang:* capability/alias layer, series
│                    identity + rank projection), loads.rs (/v1/loads fallback
│                    curation), rates.rs (reset-aware counter deltas),
│                    histogram.rs (windowed percentile estimates)
├── logtail/         log tailer, dormant in v1 (parser targets a grammar
│                    SGLang never emits; see key decision 14)
├── state/           AppState, EndpointState, ring-buffer history,
│                    bounded per-request ring (requests.rs)
├── ui/              theme (NO_COLOR aware), fleet view (endpoint table +
│                    daily-usage bar charts + 13-panel history-chart grid),
│                    endpoint detail view (rate charts + requests pane;
│                    't' = tables), help overlay, formatting widgets
└── storage/         SQLite schema, batched writes, retention; usage.rs =
                     read-only per-local-day aggregation for the fleet charts
```

## Key decisions

1. **Own Prometheus text parser** (std-only, table-tested). The format's
   traps are engine-independent — type must come from `# TYPE`, never name
   suffixes (vLLM exports a histogram named `iteration_tokens_total`;
   SGLang's counters-vs-`prompt_tokens_histogram` naming is the inverse
   trap) — plus colons in names, `+Inf` buckets, escaped label values, and
   out-of-order sample lines. A small correct parser beats a
   loosely-maintained dependency; it is validated against a real SGLang
   0.5.19 capture and lives behind `metrics::parse` so it can be swapped.
2. **Capability layer, not version checks**: `normalize.rs` maps curated
   metrics through an alias table (e.g. `sglang:inter_token_latency_seconds`
   with the pre-0.4.9 `sglang:time_per_output_token_seconds` as alias) and
   reports what it found; everything else is parsed and tolerated. Metric
   name strings live only in this file.
3. **Series identity + rank projection**: a series is
   `(model_name, engine_type, dp_rank)`, where `engine_type = "unified"`
   normalizes to none ("prefill"/"decode" are kept — PD disaggregation
   roles) and `dp_rank` appears only under DP attention. The replica ranks
   `tp_rank`/`pp_rank`/`moe_ep_rank` are **projected**: only the minimum
   rank tuple per series is curated, never summed — under
   `--enable-metrics-for-all-schedulers` every rank exports a near-identical
   copy, and summing would multiply every value by the TP size.
4. **Residual labels merge, per type**: counters and histogram buckets sum
   (`is_streaming` children of the token/request counters and the TTFT/e2e
   histograms merge by bucket-wise sum; `cache_source`, `pid` likewise);
   intensive gauges are taken only when all samples agree; extensive gauges
   (pid-fanned queue depths) sum.
5. **Finish-time vs realtime counters, never mixed**: live token rates come
   from `sglang:realtime_tokens_total{mode}` — per-forward-batch scheduler
   counters, genuinely live. The tokenizer counters
   (`prompt_tokens_total`, `generation_tokens_total`, `num_requests_total`)
   increment at request *finish* and feed only the lifetime odometers,
   completions/s, and daily usage (a long request's tokens land on its
   finish day).
6. **Loads-only fallback (two-source design)**: when `/metrics` 404s
   (server launched without `--enable-metrics`), the collector falls back to
   the always-on `GET /v1/loads` per-DP-rank gauge snapshots, curated into
   the exact shape the Prometheus path produces so state/history/recorder/UI
   need no special cases. Counter-derived values are honestly `--`, the
   endpoint gets a `LOADS` badge, the server's own throughput estimate is
   displayed with `~`, and the two sources are never merged. `/server_info`
   reporting `enable_metrics: false` upgrades the 404 into "metrics disabled
   on server — relaunch with --enable-metrics".
7. **Rates**: per-(endpoint, metric, full label set) tracker keyed on
   monotonic `Instant`. Negative delta ⇒ counter reset ⇒ rate = unavailable
   for that interval, never negative. First sample after start/reset ⇒
   unavailable.
8. **Histogram percentiles**: ring of cumulative bucket snapshots; p50/p95/p99
   estimated by linear interpolation over the *delta* between now and the
   snapshot ~window ago (default 60 s), labelled as estimates. Bucket-layout
   changes or negative deltas invalidate the window.
9. **Fleet KV aggregate**: capacity-weighted using the
   `sglang:max_total_num_tokens` gauge when every healthy endpoint exposes
   it (a directly labelled gauge — vLLM's `cache_config_info` join machinery
   is gone); otherwise per-endpoint bars plus a clearly labelled unweighted
   mean. Histogram buckets are never merged across endpoints.
10. **Zero vs unavailable**: every curated value is `Option<_>` end to end;
    missing renders as `--`, never `0`. Spec-decode gauges are value-gated:
    SGLang registers them at 0.0 when the feature is off, so ≤ 0 curates as
    absent, not as a real zero.
11. **Secrets**: TOML stores env-var *names*; values are resolved at client
    build time, live only in reqwest header maps, and are never
    Debug-printed, logged, or persisted. Displayed URLs are redacted (no
    userinfo/query).
12. **Keybindings**: `q` quit, `Tab`/`Shift+Tab` cycle views, `1`..`9` direct
    tabs (1 = fleet), `j`/`k` select row, `Enter` open endpoint, `PgUp`/`PgDn`
    scroll the embedded charts, `r` force refresh, `p` pause rendering
    (collection continues), `s` sort, `+`/`-` refresh interval, `t` endpoint
    tables, `?` help.
13. **Usage accounting (owner decision, 2026-08)**: recording is ON by
    default (`$XDG_DATA_HOME/sglangtop/usage.db`; `--no-record` opts out);
    the cumulative tokenizer counters are snapshotted every 30 s and the
    fleet tab renders past-30-days bar charts from segment-wise positive
    deltas bucketed by local day in SQL (`storage/usage.rs`) — never by
    integrating rate samples.
14. **Log tailing deferred (port decision, 2026-08)**: the vllmtop lineage
    shipped an opt-in per-request pane fed by tailing the server log. The
    tailer plumbing (rotation-aware, bounded, `log_file` config) is kept,
    but the parser was not ported: SGLang emits no per-request log lines by
    default (they need `--log-requests`), so the pane shows only guidance
    text in v1. SGLang's unconditional scheduler batch lines ("Decode
    batch, #running-req: …, gen throughput (token/s): …" every
    `--decode-log-interval` iterations; "Prefill batch, #new-seq: …") may
    feed a future activity ticker instead. The privacy firewall around the
    request ring must not regress meanwhile (see
    [LOG-TAILING.md](LOG-TAILING.md)).

15. **Liveness via `/ping`, never `/health` (port decision, 2026-08)**:
    upstream `/health` and `/health_generate` share one handler. `/health`
    short-circuits to 200 only while
    `SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION` is unset; with it set, both run
    a real one-token generation. Measured on a live 0.5.19 server: ~1.0 s per
    call and exactly +1 `realtime_tokens_total{prefill_compute}` and +1
    `{decode}`, versus ~0.1 s and zero tokens for `/ping`. Because the health
    request carries `log_metrics=False` it is invisible to the tokenizer
    counters and pollutes only the scheduler-side rates — the ones this tool
    charts — so a once-per-cycle probe made an idle server read a permanent
    ~1 token/s of the monitor's own traffic. We trade away `/health`'s 503
    "starting/shutting down" signal (startup is already visible as INIT and a
    shutdown as failing/STALE) to keep the read-only promise literal. An
    integration test asserts both endpoints receive zero requests.

## Milestones

M0–M11 built the tool against vLLM (as `vllmtop`); M12 is the SGLang port.

- [x] M0 Environment: WSL Ubuntu toolchain (Rust 1.97.1), live metrics
      captured for fixture design
- [x] M1 Vertical slice: cargo init, CLI/config, one collector, parser,
      minimal state, minimal fleet view rendering live data
- [x] M2 Metrics layer: normalization/aliases, rates + reset handling,
      histogram windows + percentiles, ring history (unit-tested)
- [x] M3 Multi-endpoint: concurrent collectors, failure isolation, dynamic
      tabs, staleness display
- [x] M4 Views: endpoint detail, History, Raw metrics (search/sort)
- [x] M5 Auth/TLS: HTTPS (rustls), bearer/header env refs, redaction
- [x] M6 Recording: SQLite (WAL, batched, dedicated thread), retention
- [x] M7 Hardening: NO_COLOR, resize, small terminals, panic-safe restore,
      bounded everything
- [x] M8 Tests: parser/rates/histogram/config unit suites, httpmock
      integration test (collect → normalize → state → recover)
- [x] M9 Docs: README, CONTRIBUTING, ARCHITECTURE, METRICS compat,
      example config
- [x] M10 CI/release: GitHub Actions (fmt, clippy -D warnings, test, build),
      x86_64 + aarch64 release workflow, installer scaffold
- [x] M11 Verification: fmt/clippy/test/release-build clean; mock + live
      read-only validation; failure-injection run (live endpoint + dead
      endpoint), NO_COLOR + 60×12 terminal runs, SIGTERM restore verified
- [x] M12 SGLang port (2026-08): collector probes `/server_info` (with the
      deprecated `/get_server_info` alias) and the `/v1/loads` fallback;
      normalize layer rewritten to `sglang:*` names with rank projection and
      residual-label merging; 13-panel chart grid, ABRT/RETR fleet columns,
      spec/PD/LOADS badges; fixtures replaced with real SGLang 0.5.19
      captures plus synthetic PD pairs; screenshot mocks ported (three
      profiles including a metrics-off edge case); docs rewritten.
      185 unit + 12 integration tests green.

## Deliberately open

- **crates.io publish: decide at release time** (`publish = false` until
  then; the crate name `sglangtop` was still free as of 2026-08-25).
- **sgl-router**: the router exports its own `sgl_router_*` metric
  namespace, which sglangtop does not understand; pointing it at a router's
  `/metrics` curates nothing.
- **PD fleet double-counting**: whether one request through a
  prefill+decode pair increments `num_requests_total` on both servers is
  unresolved; fleet aggregation currently sums all endpoints.
- **Log tailing** stays dormant until an SGLang-native source is designed
  (key decision 14).

## Decided

- **License: MIT OR Apache-2.0** (dual, Rust-ecosystem standard) — decided
  2026-08-21; LICENSE-MIT + LICENSE-APACHE are in the tree.
- **Repository: `github.com/yongzane00/sglangtop`** — resolved 2026-08-25
  from the owner's own remote; baked into Cargo.toml, README/CONTRIBUTING
  clone URLs, and the installer's default slug. Remaining release steps:
  a first tag to exercise the release workflow, and verifying the aarch64
  binary on real hardware.

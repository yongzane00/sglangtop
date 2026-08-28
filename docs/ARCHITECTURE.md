# Architecture

sglangtop is built around ownership and message passing: exactly one task owns
the application state, everything else communicates over bounded channels.

```
┌────────────────────┐   AppEvent::Scrape    ┌──────────────────────────────┐
│ collector task #1  ├──────────────┐        │ main select! loop (app.rs)   │
├────────────────────┤              │        │  owns AppState               │
│ collector task #2  ├──────────────┼───────▶│  reduce: curate → rates →    │
├────────────────────┤   bounded    │        │  histogram windows → history │
│ …one per endpoint  ├──── mpsc ────┘        │  render (throttled, dirty-   │
└────────────────────┘                       │  flag driven)                │
┌────────────────────┐   AppEvent::Key       │            │                 │
│ input thread       ├──────────────────────▶│            ▼ bounded sync    │
│ (blocking reads)   │                       │  ┌──────────────────────┐    │
└────────────────────┘                       │  │ SQLite recorder      │    │
                                             │  │ (dedicated OS thread)│    │
                                             └──┴──────────────────────┴────┘
```

Design rules:

- **No shared mutable state.** Collectors never see `AppState`; the reducer
  never does I/O; the renderer borrows state immutably.
- **Everything is bounded**: the event channel (256), the recorder queue
  (64 batches, dropped+counted when full), history rings (time window plus a
  hard point cap), histogram snapshot rings, counter banks (generation-based
  eviction), and response body sizes (8 MiB metrics, 1 MiB metadata/loads).
- **The render loop is not hot.** Draws happen on events, throttled to
  ~10 fps, with a 500 ms heartbeat tick so ages/staleness advance. Idle CPU
  is effectively zero.

## Module tour

| Module | Responsibility |
| --- | --- |
| `main.rs` | CLI → config → terminal guard → runtime → wiring. Restores the terminal on quit, SIGTERM, and panics (ratatui's panic hook). |
| `cli.rs` | clap definitions (`NAME=URL[@CAP]` endpoint grammar), shell completions. |
| `config.rs` | TOML schema, precedence merge (defaults < file < CLI), validation, duplicate-name rejection, env-reference secret resolution, URL redaction. |
| `collector/` | Two tokio tasks per endpoint. The metrics task GETs `/metrics` on an anchored cadence: non-overlapping requests, slots missed by a slow scrape are skipped (never queued), fixed quarter-second fleet phase offsets instead of random jitter. When `/metrics` returns 404 (server launched without `--enable-metrics`) it falls back to `/v1/loads` — never both sources at once. The optional-probe task GETs `/ping` every cycle and `/server_info` + `/v1/models` every 30th cycle (never `/health`/`/health_generate`: upstream they share one handler that runs a real one-token generation whenever `SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION` is set, which would make the monitor itself a permanent ~1 token/s of load); `/server_info` falls back to the deprecated `/get_server_info` alias on 404/405. Parsing runs off the UI path; per-request timeouts (3 s connect / 10 s total); no redirects. Auth headers live only inside the reqwest client. |
| `metrics/parse.rs` | Own Prometheus text parser (see below). |
| `metrics/model.rs` | `ScrapeText` / `MetricFamily` / `Sample` / `LabelSet` types. |
| `metrics/normalize.rs` | Curated extraction + capability detection + alias table. Series identity is `SeriesKey { model_name, engine_type (≠ "unified"), dp_rank }`; replica ranks (`tp_rank`/`pp_rank`/`moe_ep_rank`) are *projected* to the minimum rank tuple (summing would multiply values under `--enable-metrics-for-all-schedulers`); residual labels merge — counters and histogram buckets sum (`is_streaming`, `cache_source`, `pid`), intensive gauges agree-or-absent, extensive gauges (pid-fanned queue depths) sum. The only file that knows `sglang:*` name strings. |
| `metrics/loads.rs` | The `/v1/loads` fallback: maps SGLang's always-on per-DP-rank JSON load snapshots onto the same `CuratedScrape` shape the Prometheus path produces. Gauges only — everything counter-derived (token rates, completions/s, percentiles, daily usage) stays honestly absent. |
| `metrics/rates.rs` | Reset-aware counter deltas over a monotonic clock; `CounterBank` with generation eviction. |
| `metrics/histogram.rs` | Ring of cumulative snapshots → windowed bucket deltas → p50/p95/p99 interpolation. |
| `state/` | `EndpointState::apply(outcome)` reducer, freshness/staleness, endpoint aggregates (incl. KV capacity weighting), `/server_info`-driven `n/max` capacity and badges, ring-buffer history, bounded per-request ring (`state/requests.rs`). |
| `logtail/` | Log-tailer plumbing (poll-based 500 ms, rotation/truncation-aware, bounded) retained but **dormant in v1**: the line parser still targets vLLM's `RequestLogger` grammar, which SGLang never emits, so the requests pane only shows guidance text. See [LOG-TAILING.md](LOG-TAILING.md). |
| `ui/` | Theme (truecolor/256/mono), formatting, the fleet view (endpoint table + daily-usage bar charts + 13-panel history-chart grid), and the endpoint detail view (rate charts + requests pane by default; `t` toggles the activity/latency tables). Head badges: `spec:<ALGO>`, `PD:prefill|decode`, `LOADS`, `RESTARTED`. Pure functions of `&App`. |
| `storage/` | SQLite recorder thread: WAL, batched transactions, chunked retention cleanup, verified write counters, schema versioning. On by default (`$XDG_DATA_HOME/sglangtop/usage.db`); `--no-record` opts out. |
| `storage/usage.rs` | Read side: per-local-day usage totals (tokens in/out, requests) from 30 s cumulative-counter snapshots, computed in SQL as segment-wise positive `LAG` deltas (restart-safe) on a second read-only WAL connection via `spawn_blocking`; result cached in `App::usage`. |

### The loads-only fallback

SGLang serves `GET /v1/loads` on every build, with or without
`--enable-metrics`. When `/metrics` 404s, the collector ships those gauge
snapshots instead of going dark: running/waiting, KV token usage and
capacity, cache hit rate (the server's rolling window), the server's own
gen-throughput estimate (displayed with a `~` prefix), spec-accept stats,
and PD queues. The endpoint gets a `LOADS` badge, counter-derived panels
show `--`, and when `/server_info` additionally reports
`enable_metrics: false` the 404 error text upgrades to "metrics disabled on
server — relaunch with --enable-metrics". The two sources are never merged.

## Why an own Prometheus parser

The exposition format's traps are engine-independent: metric type must come
from `# TYPE`, never from name suffixes (vLLM famously exports a *histogram*
named `iteration_tokens_total`; SGLang's `prompt_tokens_histogram` is the
inverse trap), plus colons in metric names, `+Inf` bounds, escaped label
values, and out-of-order sample lines within a family (the live SGLang
capture emits `per_stage_req_latency_seconds_sum` before its buckets). The
maintained parser crates were last released in 2023; rather than depend on
an unmaintained crate, ~300 table-tested lines in `metrics/parse.rs`
implement the text format with the same leniencies as Prometheus's own
scrape-time parser (blanks between tokens, trailing label commas,
out-of-order families, unknown-escape pass-through, `# EOF` tolerated), and
are validated against a real SGLang 0.5.19 capture in `tests/fixtures/`.
Invalid lines are skipped and reported per line, never failing a whole
scrape. Everything downstream sees only `ScrapeText`, so the parser is
swappable.

## Correctness decisions

- **Zero vs unavailable**: every curated value is `Option<f64>` from
  normalization to rendering. `--` in the UI is `None`, never `0`.
- **Rates**: `rate = Δcounter / Δmonotonic-time` per full label set. A
  negative delta ⇒ counter reset ⇒ that interval reports `Reset` (rate
  unavailable, `RESTARTED` badge) and the tracker restarts. First samples
  report `Unavailable`. NaN poisons a tracker until fresh samples arrive.
- **Live rates come from scheduler counters**: prompt/generation tokens/s
  are rates of `sglang:realtime_tokens_total{mode}` (incremented per forward
  batch); the tokenizer counters (`prompt_tokens_total`, …) increment only
  at request *finish* and are used for lifetime odometers and daily usage,
  never for live rates.
- **Percentiles**: cumulative buckets are snapshotted per scrape
  (`is_streaming` children of one family are first merged by bucket-wise
  sum); estimates interpolate within the bucket where the target quantile
  falls, using the delta between the newest snapshot and the oldest one
  within the window. Bucket-layout changes or resets clear the ring.
  `+Inf`-bucket hits saturate at the highest finite bound. "No observations
  in the window" is reported as zero observations — distinct from "cannot
  estimate".
- **Aggregation honesty**: fleet KV is capacity-weighted only when every
  series exposes `sglang:max_total_num_tokens` (a directly labelled gauge —
  no join machinery, unlike vLLM's `cache_config_info`); otherwise the UI
  shows an unweighted mean explicitly labelled as such. TTFT across a fleet
  row is shown as *worst* p95, labelled. Histogram buckets are never merged
  across endpoints (bucket compatibility across servers is not assumed
  anywhere in v1).
- **Staleness**: no successful scrape for `max(3 × interval, 5 s)` ⇒
  `STALE`; the last good snapshot stays on screen, and fleet totals exclude
  stale endpoints rather than silently counting frozen numbers.
- **Pause**: freezes automatic redraws only; collection, reduction, history,
  and recording continue. Documented in the README.

## Secret handling

- TOML stores env-var **names** (`bearer_token_env`, `header_env`); values
  are read once at startup.
- `ResolvedAuth` implements neither `Debug` nor `Clone`; resolved values
  flow only into `reqwest` default headers, which are marked
  `set_sensitive(true)`.
- Displayed URLs pass through `redact_url` (no userinfo, no query).
- HTTP errors are reduced to the innermost source message (e.g.
  "Connection refused"); reqwest's URL-bearing display form is never used.
- The redirect policy is `none`, so auth headers cannot follow a redirect
  to another origin.
- The recorder schema has no column that could hold a secret.

## Recording schema

```sql
CREATE TABLE meta    (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- meta rows: ('schema_version', '1')

CREATE TABLE samples (
    ts_ms    INTEGER NOT NULL,   -- wall-clock ms since epoch
    endpoint TEXT    NOT NULL,   -- configured endpoint name
    model    TEXT    NOT NULL,   -- model_name label
    engine   TEXT,               -- SeriesKey::engine_str(): "prefill",
                                 -- "dp0", "prefill dp0", or NULL
    metric   TEXT    NOT NULL,   -- state::series_id (running, waiting, …)
    value    REAL    NOT NULL
);
CREATE INDEX idx_samples_ts           ON samples (ts_ms);
CREATE INDEX idx_samples_ep_metric_ts ON samples (endpoint, metric, ts_ms);
```

Migration strategy: `schema_version` is stamped at creation. A database with
a **newer** version than the binary understands is refused (no destructive
surprises); older versions will be migrated in place by future releases
(none exist yet). Rows are the same curated series ids the fleet
charts, written in one transaction per scrape batch, WAL mode, `synchronous
= NORMAL`. Retention deletes in chunks of 10 000 rows at most every 10
minutes.

## Notes for reviewers

Areas worth extra scrutiny:

1. `metrics/histogram.rs` — quantile interpolation edge cases (empty
   windows, single-bucket layouts, saturation).
2. `collector/mod.rs` — the anchored scheduler (`next_slot_strictly_after`
   / `wait_for_start`): force-refresh and interval-change interaction;
   confirm no busy-loop path when channels close.
3. `metrics/normalize.rs` — rank projection vs residual-label merging; a
   label that should be projected but gets summed multiplies every value.
4. Fleet totals excluding stale endpoints (`ui/fleet.rs`) — intended, but
   opinionated.
5. The recorder's drop-on-full policy and its `rows_written` verification
   counter.

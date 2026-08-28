# Daily usage accounting

How the fleet view's "past 30 days" bar charts (output tokens/day, input
tokens/day, requests/day) are computed, and why that specific design. Code:
`src/storage/usage.rs` (query), `src/state/mod.rs::cumulative_samples`
(recording), `src/ui/fleet.rs::draw_daily_usage` (rendering).

## Why not integrate the recorded rates?

The recorder has always stored per-second rates (`prompt_tps`, …) at scrape
cadence. Summing `rate × Δt` over a day looks tempting but silently
undercounts:

- the recorder's queue is bounded and **drops batches** under pressure;
- rates are `None` for one interval after every counter reset;
- any sglangtop downtime is a hole in the integral.

All three failure modes *lose* data with no way to notice. Cumulative
counter **snapshots** invert the failure mode: a lost snapshot just widens
the gap between two surviving ones, and the difference still covers the
whole span. Snapshots self-heal; integrals don't.

## What is recorded

Every 30 s per endpoint (`COUNTER_RECORD_EVERY`, first successful scrape
records immediately), three extra rows go into the same `samples` table:

| metric | source |
| --- | --- |
| `prompt_tokens_total` | Σ over `is_streaming` of `sglang:prompt_tokens_total` |
| `generation_tokens_total` | Σ over `is_streaming` of `sglang:generation_tokens_total` |
| `requests_total` | Σ over `is_streaming` of `sglang:num_requests_total` |

These ids live in `state::series_id::CUMULATIVE`, deliberately **not** in
`series_id::ALL`, so they get no in-memory chart rings. 30 s cadence keeps
them at roughly 0.3 % of the recorder's row volume while bounding every
error term below at ≤ 30 s of traffic.

Two SGLang-specific caveats. These are *tokenizer* counters, incremented at
request **finish**: a long-running request's tokens all land on the day it
completes, not the days it streamed. And an endpoint in the loads-only
fallback (`/metrics` disabled, gauges via `/v1/loads`) exposes no cumulative
counters at all — it records nothing here, and its days honestly stay `--`.

## The aggregation query

One SQL pass over the last 31 days of those rows:

```sql
value - LAG(value) OVER (
    PARTITION BY endpoint, model, engine, metric ORDER BY ts_ms
)                                   -- consecutive deltas per series
...
SELECT day, metric, SUM(MAX(d, 0.0))  -- clamp negatives, sum per local day
GROUP BY strftime('%Y-%m-%d', ts_ms/1000, 'unixepoch', 'localtime'), metric
```

**Why this is correct:** within a monotonic run of one counter, consecutive
deltas telescope — their sum is exactly `last − first` (segment max−min).
So summing *positive* deltas equals summing per-segment totals, split at
day boundaries by attributing each delta to the day of its **later**
sample.

**Restarts:** the delta that straddles a counter reset is negative and
clamps to 0. Example: `100 → 500 → restart → 50 → 200` yields
`400 + 150 = 550`; the 50 tokens between restart and first snapshot are
unobservable and dropped. Loss is bounded by one cadence on each side of
the restart.

**Day boundaries:** the partition is deliberately *not* split by day —
splitting would silently drop one delta every midnight. Instead the
midnight-straddling delta lands wholly in the later day (error ≤ one
cadence per boundary).

**Absent ≠ zero:** a series with a single sample yields no delta → that
day's field stays `None` and renders `--`. A flat counter yields a real 0.
The two must never be conflated (project-wide rule).

## The calendar axis

`days` always contains exactly 30 entries, one per local calendar day,
generated **by SQLite** (`strftime(..., 'localtime')` over a recursive
CTE). Std Rust has no timezone database and the project takes no chrono
dependency; letting SQLite own all date math keeps bucketing and axis
generation consistent, including 23/25-hour DST days. Tests derive expected
day strings by asking SQLite the same question, so they pass in any
timezone.

## Error bounds (summary)

| Event | Worst-case error |
| --- | --- |
| Server restart | ≤ 2 × 30 s of traffic lost (around the reset) |
| Day boundary | ≤ 30 s of traffic attributed to the neighbor day |
| Dropped recorder batch | none (next snapshot covers the span) |
| sglangtop offline for a gap | usage during the gap lands on the day observation resumed; a restart *inside* the gap loses the pre-restart segment |

For daily bars these are negligible; they are documented rather than
"fixed" because exact accounting is impossible from sampled counters.

## Delivery to the UI

Reads never run on the UI thread and never touch the writer: a second
**read-only** SQLite connection (WAL guarantees non-blocking readers) runs
in `tokio::task::spawn_blocking`, and the result arrives as
`AppEvent::UsageLoaded` into `App::usage` — startup, every 5 min, and on
`r`. A failed query keeps the last good data and shows the error; recording
disabled shows *why* (`--no-record`, unresolvable default path, or recorder
startup failure) instead of empty axes.

## Rendering rules (`draw_daily_usage`)

- Bars size dynamically: `bar_width = max(1, inner_width/30 − gap)`, gap 1
  only when ≥ 3 cells per day. If even width-1 bars don't fit 30 days, the
  most recent N days render and the title says `(last Nd)` — never a
  silently cropped axis.
- Unobserved days draw a zero-height bar styled as unavailable with a `--`
  text value; per-bar value labels appear only at `bar_width ≥ 5`,
  day-of-month labels at `≥ 2`.
- Titles carry the window total (`Σ 8.85M`) computed only from observed
  days.

# Metric compatibility

sglangtop detects capabilities from what an endpoint actually exposes — there
are no version checks. This table lists the curated metrics, their sources,
and what happens when they are missing. Everything else the server exposes is
parsed and tolerated; curation simply ignores it.

Ground truth: a live SGLang 0.5.19 capture
(`tests/fixtures/sglang_unified_single.txt`, Qwen/Qwen3.8-27B-FP8 with DFLASH
speculative decoding) plus the SGLang metrics collector source
(`python/sglang/srt/observability/metrics_collector.py`) as of August 2026.
See `docs/SGLANG-SEMANTICS.md` for the verified upstream behaviors this
mapping relies on.

## Labels and series identity

SGLang exports two families of series:

- **Scheduler metrics** carry `model_name`, `engine_type`
  (`prefill`/`decode`/`unified`), `tp_rank`, `pp_rank`, `moe_ep_rank`, and
  `dp_rank` only when DP attention is on. Some gauges also carry a `pid`
  (prometheus multiprocess child).
- **Tokenizer (frontend) metrics** carry only `model_name` and `engine_type`,
  plus fan-out labels: `is_streaming` on token/request counters and the
  TTFT/e2e histograms, `cache_source` on `cached_tokens_total`.

sglangtop keys every series by `(model_name, engine_type, dp_rank)`, where
`engine_type = "unified"` normalizes away (so scheduler and tokenizer metrics
of a plain server merge into one series). Replica ranks
(`tp_rank`/`pp_rank`/`moe_ep_rank`) are **projected**: only the smallest rank
tuple per series is curated, never summed — under
`--enable-metrics-for-all-schedulers` every rank exports a near-identical
copy, and summing would multiply every value by the TP size. Residual labels
merge instead: counters and histogram buckets sum; intensive gauges are taken
only when all samples agree; extensive gauges (`pid`-fanned queue depths) sum.

Under DP attention, scheduler series exist per `dp_rank` while tokenizer
metrics stay on one un-ranked "frontend" series — the field sets are
disjoint, so endpoint sums never double-count.

## Curated metrics

| Quantity | Source (exact name) | Notes | When missing |
| --- | --- | --- | --- |
| Running requests | `sglang:num_running_reqs` | gauge | `--` |
| Waiting requests | `sglang:num_queue_reqs` | gauge; grammar/paused/retracted queue gauges shown as extras when non-zero | `--` |
| Aux queue depths | `sglang:num_grammar_queue_reqs`, `sglang:num_paused_reqs`, `sglang:num_retracted_reqs` | extensive gauges, `pid` children summed | extras hidden |
| KV token usage (0–1) | `sglang:token_usage` | gauge, the server's own pool ratio | `--`, no bar |
| KV used tokens | `sglang:num_used_tokens` | gauge, direct | absolute figure falls back to usage × capacity |
| KV capacity (tokens) | `sglang:max_total_num_tokens` | static gauge with full labels — no join machinery (unlike vLLM's `cache_config_info`) | fleet KV falls back to labelled unweighted mean |
| Hybrid pool usage | `sglang:full_token_usage`, `sglang:swa_token_usage`, `sglang:mamba_usage` | value-gated: 0 = pool absent | rows hidden |
| Prompt tokens/s | `rate(sglang:realtime_tokens_total{mode="prefill_compute"}) + {mode="prefill_cache"}` | per-batch scheduler counters — genuinely live | `--` |
| Generation tokens/s | `rate(sglang:realtime_tokens_total{mode="decode"})` | fallback: `sglang:gen_throughput` gauge, displayed `~N (server est)` | `--` |
| Completions/s | `Σ_is_streaming rate(sglang:num_requests_total)` | increments at request **finish**; family-consistent reset rule | `--` |
| Aborts | `sglang:num_aborted_requests_total` | not part of `num_requests_total`; SGLang has no error/finished-reason taxonomy | `--` (counter materializes on first abort) |
| Retractions | `sglang:num_retracted_requests_total` | decode-OOM requeue — the preemption analogue | `--` (materializes on first retraction) |
| Cache hit rate | `sglang:cache_hit_rate` | the SERVER's rolling-window ratio, charted and labelled "(srv win)" | `--` |
| Cached-token share | `Δ Σ_source sglang:cached_tokens_total ÷ Δ Σ_stream sglang:prompt_tokens_total` | our scrape-window share, both finish-time attributed | `--` |
| Spec accept rate/length | `sglang:spec_accept_rate`, `sglang:spec_accept_length` | registered-at-zero upstream: value ≤ 0 curates as absent | rows/chart hidden |
| TTFT | `sglang:time_to_first_token_seconds` (histogram) | `is_streaming` children merged by bucket-wise sum | `--` |
| Inter-token latency | `sglang:inter_token_latency_seconds` (histogram) | alias: `sglang:time_per_output_token_seconds` (pre-0.4.9) | `--` |
| E2E latency | `sglang:e2e_request_latency_seconds` (histogram) | `is_streaming` merged | `--` |
| Queue time | `sglang:queue_time_seconds` (histogram) | scheduler-side | `--` |
| Stage latencies | `sglang:per_stage_req_latency_seconds{stage}` | 11-stage allowlist → one canonical id each; rows appear only when observed | row absent (never a fake `--` row) |
| Prompt tokens/request | `sglang:prompt_tokens_histogram` | | `--` |
| Generation tokens/request | `sglang:generation_tokens_histogram` | | `--` |
| Uncached prompt tok/request | `sglang:uncached_prompt_tokens_histogram` | row appears only when observed | row absent |
| PD queues | `sglang:num_prefill_bootstrap_queue_reqs`, `sglang:num_prefill_inflight_queue_reqs`, `sglang:num_decode_prealloc_queue_reqs`, `sglang:num_decode_transfer_queue_reqs` | kept only on `prefill`/`decode` series (unified registers them at 0) | rows hidden |
| KV transfer latency | `sglang:kv_transfer_latency_ms` (histogram) | exported in **milliseconds**, curated in seconds | row absent |

Adding an alias is a one-line change in `src/metrics/normalize.rs` — that
file is the only place metric name strings exist.

## Daily usage (SQLite recorder)

`prompt_tokens_total`, `generation_tokens_total`, and `requests_total` are
snapshots of the tokenizer counters summed over `is_streaming`. They increment
at request **finish** (lump attribution): a request that runs across midnight
lands its tokens entirely on its finish day. Aborted requests are not counted.

## The /v1/loads fallback

When `/metrics` returns 404 (server launched without `--enable-metrics`),
sglangtop automatically polls `GET /v1/loads` instead — SGLang serves it
unconditionally. One series per DP-rank snapshot carries: running, waiting,
token usage, used tokens, capacity, cache hit rate, gen throughput (server
estimate, shown `~`), spec accept stats, aux queues, and PD queues. Nothing
counter-derived exists in this mode, so token rates, completions/s, latency
percentiles, and daily usage are honestly `--`. The endpoint is badged
`LOADS`; the two sources are never merged.

## Known traps handled

- **`sglang:` names contain a colon** — legal Prometheus, but some tools
  choke on it. The built-in parser handles it natively.
- **Type comes from `# TYPE` only.** `sglang:realtime_tokens_total` is a
  counter and `sglang:prompt_tokens_histogram` is a histogram; suffixes lie.
- **Counters materialize lazily.** Labelled counters (aborts, retractions)
  emit no samples until the first event; absence means "none yet", shown `--`.
- **Live NaN gauges exist** (`sglang:fwd_occupancy` while idle); non-finite
  values are skipped, never propagated.
- `_created` companion series (from `prometheus_client`) are separate gauge
  families; curation ignores them.
- Summaries without quantiles parse fine.
- `python_*`, `process_*`, `sglang:http_*` families are parsed but not
  displayed — sglangtop deliberately does not become a host monitor.
- Backend/feature-specific families (grammar, EPLB, HiCache, encoder,
  MFU estimates, `sglang:func_latency_seconds`) and future metrics are
  tolerated without breaking anything.

## Not curated on purpose

`sglang:utilization` (can be a −1 sentinel), `sglang:new_token_ratio`,
`sglang:decode_sum_seq_lens`, `sglang:kv_available_tokens` /
`kv_evictable_tokens` (the used/capacity pair covers the display),
`sglang:context_len` / `page_size` / `num_pages` / startup gauges (static
config — `/server_info` is the better source), `sglang:func_latency_seconds`
(labelled only by function name — cannot join to a model series),
HiCache/storage/encoder/EPLB internals, and the `sgl-router`'s separate
`sgl_router_*` namespace (a router is not an SGLang server; pointing
sglangtop at one yields honest `--` plus a working `/v1/models`).

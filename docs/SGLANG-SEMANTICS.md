# SGLang semantics

Verified facts about SGLang's observability surface that sglangtop's design
relies on. Sources: `sgl-project/sglang` @ `main` (2026-08-25, ≈ v0.5.18),
especially `python/sglang/srt/observability/metrics_collector.py`,
`managers/tokenizer_manager.py`, `entrypoints/http_server.py`,
`entrypoints/v1_loads.py`; plus live captures from a real 0.5.19.dev server
(now in `tests/fixtures/`). When upstream changes, re-verify against these
files before changing `normalize.rs`.

## Exposure

- Prometheus metrics exist **only with `--enable-metrics`**, served at
  `/metrics` on the main API port (default 30000). No separate metrics port.
- The prefix is **`sglang:` with a colon** (not `sglang_`). Legal Prometheus;
  some third-party tools mishandle it, ours does not.
- `prometheus_client` multiprocess mode: scheduler subprocesses write to a
  shared directory; some gauges therefore fan out over a `pid` label, and
  stale children from dead processes are possible within one server lifetime.
- `GET /v1/loads` is registered **unconditionally** — it works without
  `--enable-metrics` and returns one JSON `LoadSnapshot` per DP rank
  (running/waiting, token usage, capacity, cache hit rate, gen throughput,
  spec stats, PD queues, memory breakdown). This is sglangtop's fallback
  source, and the reason the tool is useful against metrics-off servers.

## Labels

- Scheduler collector labels: `model_name`, `engine_type`, `tp_rank`,
  `pp_rank`, `moe_ep_rank`, plus `dp_rank` only when DP attention is on,
  plus `priority` when priority scheduling is on and any operator-defined
  `--extra-metric-labels`.
- Tokenizer collector labels: **only** `model_name` and `engine_type` (plus
  fan-outs: `is_streaming`, `cache_source`).
- `engine_type` comes from the disaggregation mode: `prefill`, `decode`, or
  `unified` (the default). There is no `pd_role` label.
- Reporting gate: by default only the `attn_tp_rank == 0` scheduler reports;
  with `--enable-metrics-for-all-schedulers` every TP rank exports
  near-identical duplicates. **Never sum across ranks.**

## Counter attribution — the trap that shapes the rate design

- `sglang:prompt_tokens_total`, `sglang:generation_tokens_total`, and
  `sglang:num_requests_total` are all incremented in
  `observe_one_finished_request` — i.e. **in a lump at request finish**.
  A 10-minute generation contributes 0 tokens/s for 10 minutes and then a
  spike. They are correct for lifetime odometers and daily usage, wrong for
  live rates.
- Live rates come from `sglang:realtime_tokens_total{mode}` instead
  (scheduler-side, incremented per forward batch):
  `prefill_compute` + `prefill_cache` = prompt tokens admitted;
  `decode` = tokens generated.
- `sglang:num_aborted_requests_total` increments separately in
  `observe_one_aborted_request`; aborted requests are **not** included in
  `num_requests_total`. SGLang has no finished-reason taxonomy.
- Labelled counters materialize lazily: no aborts yet ⇒ no
  `num_aborted_requests_total` samples at all.

## Gauges

- `sglang:token_usage` is a 0–1 fraction of the KV token pool;
  `sglang:max_total_num_tokens` is the pool capacity in tokens as a plain
  gauge with full labels — capacity needs no join.
- `sglang:cache_hit_rate` is a **server-side rolling-window** ratio
  (`recent_cache_hit_rate`), not lifetime. Chart it as-is, label the window.
- `sglang:gen_throughput` = generated tokens ÷ gap since the last decode-log
  window (`--decode-log-interval`, default 40 iterations). A server-side
  estimate that freezes at its last value when idle — display-only fallback.
- Spec-decode and PD gauges are **registered unconditionally** and read 0.0
  when the feature is off: presence of the name is not a capability signal,
  the value is. When active, `spec_accept_length ≥ 1` by construction.
- Live servers emit `NaN` gauges (`sglang:fwd_occupancy` while idle).

## Histograms

- Frontend histograms (`time_to_first_token_seconds`,
  `e2e_request_latency_seconds`) fan out over `is_streaming`; children share
  the bucket layout, so merging is a bucket-wise sum of cumulative counts.
  `inter_token_latency_seconds` has no `is_streaming`.
- `sglang:per_stage_req_latency_seconds{stage}` observed stage values:
  `request_process`, `prefill_forward`, `chunked_prefill`,
  `prefill_bootstrap`, `prefill_transfer_kv_cache`, `decode_prepare`,
  `decode_bootstrap`, `decode_waiting`, `decode_transferred` (plus internal
  `fake_output`, `quick_finish`). There are **no per-request
  prefill/decode/inference-time histograms** like vLLM's.
- `sglang:kv_transfer_latency_ms` is in **milliseconds**.
- Rename history that matters: `sglang:time_per_output_token_seconds` →
  `sglang:inter_token_latency_seconds` at v0.4.9 (alias kept).

## HTTP endpoints

- `/ping`: unconditional `Response(status_code=200)` — the only free liveness
  endpoint, and the one sglangtop polls.
- `/health` and `/health_generate`: **the same handler**. `/health`
  short-circuits to 200 only while `SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION`
  is unset; with it set — and it is, on real deployments — both run a real
  one-token generation (`input_ids=[0]`, `max_new_tokens=1`). Measured on a
  live 0.5.19 server: ~1.0 s per call and exactly +1
  `realtime_tokens_total{prefill_compute}` and +1 `{decode}`, versus ~0.1 s
  and zero tokens for `/ping`. Because that request carries
  `log_metrics=False`, it never touches the tokenizer counters — it corrupts
  only the scheduler-side rates, i.e. precisely the ones a monitor charts, so
  an idle server appears to sustain ~1 token/s that is really the monitor's
  own traffic. sglangtop therefore never calls either endpoint, and an
  integration test enforces it. Both also return 503 while the server is
  starting or shutting down, which is the signal being traded away.
- `/server_info` (deprecated alias `/get_server_info`, still needed on some
  builds): `asdict(ServerArgs)` + `version` + scheduler facts. Hundreds of
  fields; deserialize defensively. Includes `max_running_requests`,
  `max_total_num_tokens`, `speculative_algorithm`, `enable_metrics`,
  `dp_size`, `tp_size`. Body grows ~30 KiB per DP rank (`internal_states`).
  `disaggregation_mode` is the **string** `"null"` when unified.
- `/v1/models`: standard OpenAI ModelList; one card per served model, plus
  one per LoRA adapter when `--enable-lora`.
- `/v1/loads`: see Exposure above; also accepts `?format=prometheus`, which
  emits a **different** underscore-prefixed namespace — do not conflate it
  with `/metrics`.

## Console logs (for the deferred activity ticker)

Every `--decode-log-interval` (default 40) decode iterations, on the stats
rank only:

```text
Decode batch, #running-req: 233, #token: 370959, token usage: 0.82, cuda graph: True, gen throughput (token/s): 4594.01, #queue-req: 317
Prefill batch, #new-seq: 4, #new-token: 3072, #cached-token: 1024, token usage: 0.41, #running-req: 12, #queue-req: 3, ...
```

Older builds used `Decode batch. ` (period); the graph label is
device-dependent (`cuda graph` / `cpu graph` / `npu graph`). Per-request log
lines exist only with `--log-requests`. Neither grammar is parsed yet.

## Deployment modes

- **DP attention** (`dp_size > 1`): one `/metrics` endpoint, series fan out
  over `dp_rank`; tokenizer metrics stay on one un-ranked frontend series.
  `/v1/loads` always returns one snapshot per rank.
- **PD disaggregation**: prefill and decode are separate server processes
  with separate ports — separate sglangtop endpoints — distinguished by
  `engine_type`. PD queue gauges and `kv_transfer_*` histograms appear there.
  Open question (needs a live PD deployment): whether one request increments
  `num_requests_total` on both roles' frontends; if so, fleet request sums
  over a PD pair double-count.
- **sgl-router**: exposes its own `/metrics` with an `sgl_router_` prefix —
  a different namespace sglangtop does not curate.

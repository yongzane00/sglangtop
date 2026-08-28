# FAQ

Conceptual questions that shaped sglangtop's design, with the honest answers
the tool is built around.

## Where does the KV cache live, and what happens to it after a conversation?

The KV cache is accelerator (GPU) memory on the SGLang server holding
per-token attention key/value tensors. It is not on the client, and it is
not a conversation store: when a request finishes, its tokens' KV may
linger in SGLang's radix (prefix) cache until evicted, so a follow-up
request sharing the same prefix reuses it — visible only as the
`cache_hit_rate` gauge, never as content. If a "conversation" spans
multiple requests, each request re-sends the history and re-fills cache
(modulo prefix hits). See [SGLANG-SEMANTICS.md](SGLANG-SEMANTICS.md).

## Can sglangtop show who is using my SGLang server?

No — and not because sglangtop chooses not to. SGLang's metrics carry no
user, session, or API-key labels, and its HTTP API exposes no request
registry. Per-user accounting requires a gateway/proxy in front of SGLang
that authenticates callers — a different tool by design.

## Can I capture conversations for fine-tuning or a knowledge base?

People do build such pipelines, but at the serving layer (a logging proxy,
or SGLang's own `--log-requests` server flag), not at the monitoring layer.
sglangtop deliberately never persists prompts or outputs, and never proxies
or inspects inference traffic. If you build capture into your serving
stack, treat it as what it is: collection of potentially personal data,
with the consent and retention obligations that implies.

## Why does an endpoint show a LOADS badge and mostly `--`?

Its server was launched without `--enable-metrics`, so `/metrics` 404s and
sglangtop falls back to `GET /v1/loads` — always-on gauge snapshots
(running/waiting, KV usage and capacity, cache hit rate, the server's own
throughput estimate shown as `~N tokens/s`, spec-accept stats, PD queues).
Everything counter-derived — token rates, completions/s, latency
percentiles, daily usage — cannot be computed from gauges and is honestly
`--`. Relaunch the server with `--enable-metrics` for the full picture; the
two sources are never merged.

## Why does the UI say `tokens/s` instead of `t/s`?

`t/s` reads as a physics unit and is ambiguous to engineers. Similarly,
request-count chart axes use integer bounds (a y-axis label of "1.1
requests" is nonsense), and rates that cannot be computed yet show `--`
rather than a fake 0.

## What's the difference between `--` and `0`?

`--` means *unavailable*: the endpoint didn't expose the metric, the first
sample hasn't landed, a counter just reset, or a day has no recorded
observations. `0` always means a measured zero. The distinction is carried
end-to-end (`Option` in every layer) because conflating them turns "I don't
know" into "nothing happened" — the worst kind of monitoring lie.

## What exactly do `served` and the token odometer count?

`served` is the sum of SGLang's `num_requests_total` counter across its
`is_streaming` children — finished requests only; aborts are a separate
counter (`num_aborted_requests_total`) and are *not* included. `tokens X in
/ Y out` are the cumulative tokenizer prompt/generation counters. All are
"since the server started" — they reset honestly on a restart — and they
increment at request *finish* time: a long-running request's tokens all
land the moment it completes, not while it streams. Live tokens/s comes
from the per-batch scheduler counters instead, which is why the odometer
and the rate charts can disagree moment to moment.

## Why do the daily-usage charts need recording? Why can bars show `--`?

30 days doesn't fit in memory (the in-memory history rings cap at ~68
minutes at 1 s refresh). The bars are computed from the SQLite recorder,
which is on by default; a `--` bar means "sglangtop wasn't watching that day"
— absent is not zero. Endpoints in the loads-only fallback expose no
cumulative counters at all, so they contribute nothing to the daily charts.
Details and error bounds: [USAGE-ACCOUNTING.md](USAGE-ACCOUNTING.md).

## Why is the requests pane empty?

Because per-request visibility is deferred in v1: SGLang emits no
per-request log lines by default, and the log parser sglangtop inherited
targets a grammar SGLang never writes, so the pane only shows guidance
text. See [LOG-TAILING.md](LOG-TAILING.md) for what SGLang *does* log and
what a future version may parse.

## Why do peaks (`pk`) sometimes disappear?

Session peaks are discarded when the monitored server restarts — a peak
from before a restart would be a stale brag about a process that no longer
exists.

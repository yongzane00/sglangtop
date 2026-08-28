# Log tailing (deferred in v1)

The requests pane and its log tailer are **dormant** in the SGLang port.
The tailer plumbing survives intact — poll-based (500 ms), rotation- and
truncation-aware, bounded in bytes/lines/events, `log_file` still accepted
per endpoint in the config — but the line parser (`src/logtail/parse.rs`)
still implements the old vLLM `RequestLogger` grammar, which SGLang never
emits. Pointing `log_file` at an SGLang log therefore parses nothing, and
the requests pane only ever shows its guidance text.

## Why it was deferred rather than ported

SGLang has no per-request log lines by default; they exist only behind the
`--log-requests` server flag (with `--log-requests-level` controlling how
much request content is dumped). What SGLang *does* log, unconditionally,
are scheduler batch lines:

```
Decode batch, #running-req: 2, #token: 23952, token usage: 0.18, gen throughput (token/s): 98.53, #queue-req: 0
Prefill batch, #new-seq: 1, #new-token: 512, #cached-token: 384, ...
```

The decode line appears every `--decode-log-interval` iterations (default
40). These are batch aggregates, not request events — they cannot populate
a per-request table. A future version may parse them into an activity
ticker instead; the per-request pane would then require `--log-requests`
and a new parser.

## What must not regress meanwhile

The privacy rules attached to the tailer still hold and must survive any
future revival: anything parsed from logs lives only in the bounded
in-memory ring (`src/state/requests.rs`), is never written to the SQLite
recorder (the recorder's only inputs are `EndpointState::current_samples()`
and `cumulative_samples()`, which cannot see the ring), never
`tracing`-logged, and never persisted. Tailing stays read-only, opt-in per
endpoint, local files only.

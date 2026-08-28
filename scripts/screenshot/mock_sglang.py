#!/usr/bin/env python3
"""A stdlib-only mock SGLang server for screenshots and local UI testing.

Serves /metrics (Prometheus text, SGLang 0.5.19-era names and label shapes),
/health, /server_info, /v1/models, and /v1/loads. Values evolve smoothly with
wall time so sglangtop's charts, rates, and percentile estimates all show
believable, moving data.

The "edge" profile simulates a server launched WITHOUT --enable-metrics:
/metrics 404s and /v1/loads carries the gauges (sglangtop's fallback mode).

Everything is deterministic given wall time; counters are monotonic.
Only sanitized example model names are used.
"""

import argparse
import json
import math
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

START = time.monotonic()

SCHED = 'engine_type="unified",model_name="{m}",moe_ep_rank="0",pp_rank="0",tp_rank="0"'
FRONT = 'engine_type="unified",model_name="{m}"'


def snapshot(cfg):
    """Current instantaneous values, shared by /metrics and /v1/loads."""
    t = time.monotonic() - START

    def mono(rate, period, phase=0.0):
        # Monotonic counter: base rate plus a bounded wave (derivative >= 0).
        amp = rate * 0.6
        return rate * t + amp * period * (1 - math.cos(t / period + phase))

    running = max(
        0.0,
        round(
            cfg["run_base"]
            + cfg["run_amp"] * math.sin(t / 13 + cfg["phase"])
            + 0.9 * math.sin(t / 5 + cfg["phase"] * 2)
        ),
    )
    waiting = max(0.0, round(1.6 * math.sin(t / 29 + cfg["phase"]) - 0.9))
    usage = min(0.97, max(0.02, cfg["kv_base"] + cfg["kv_amp"] * math.sin(t / 23 + cfg["phase"])))
    hit = min(0.95, max(0.0, cfg["hit_rate"] + 0.08 * math.sin(t / 31 + cfg["phase"])))
    return dict(
        t=t,
        mono=mono,
        running=running,
        waiting=waiting,
        usage=round(usage, 4),
        used_tokens=round(usage * cfg["capacity"]),
        hit_rate=round(hit, 4),
        gen_tps_gauge=round(max(0.0, cfg["gen_tps"] * (1 + 0.4 * math.sin(t / 9))), 2),
    )


def build_metrics(cfg):
    s = snapshot(cfg)
    mono = s["mono"]
    m = cfg["model"]
    sched = SCHED.format(m=m)
    front = FRONT.format(m=m)
    out = []

    def gauge(name, value, labels=sched):
        out.append(f"# TYPE {name} gauge")
        out.append(f"{name}{{{labels}}} {value}")

    gauge("sglang:num_running_reqs", s["running"])
    gauge("sglang:num_queue_reqs", s["waiting"])
    gauge("sglang:token_usage", s["usage"])
    gauge("sglang:num_used_tokens", s["used_tokens"])
    gauge("sglang:max_total_num_tokens", cfg["capacity"])
    gauge("sglang:cache_hit_rate", s["hit_rate"])
    gauge("sglang:gen_throughput", s["gen_tps_gauge"])
    if cfg["spec"]:
        gauge("sglang:spec_accept_length", round(2.6 + 0.6 * math.sin(s["t"] / 37), 3))
        gauge("sglang:spec_accept_rate", round(0.55 + 0.1 * math.sin(s["t"] / 41), 3))

    # Scheduler realtime counters: the live token-rate source.
    out.append("# TYPE sglang:realtime_tokens_total counter")
    for mode, rate, period in (
        ("prefill_compute", cfg["prompt_tps"] * (1 - cfg["hit_rate"]), 17),
        ("prefill_cache", cfg["prompt_tps"] * cfg["hit_rate"], 17),
        ("decode", cfg["gen_tps"], 11),
    ):
        out.append(
            f'sglang:realtime_tokens_total{{{sched},mode="{mode}"}} {round(mono(rate, period), 1)}'
        )

    out.append("# TYPE sglang:num_retracted_requests_total counter")
    out.append(f"sglang:num_retracted_requests_total{{{sched}}} {int(s['t'] / 240)}")

    # Tokenizer counters, fanned out over is_streaming (finish-time totals).
    total_reqs = mono(cfg["req_rate"], 19)
    for name, rate in (
        ("sglang:prompt_tokens_total", cfg["prompt_tps"]),
        ("sglang:generation_tokens_total", cfg["gen_tps"]),
    ):
        out.append(f"# TYPE {name} counter")
        v = mono(rate, 17)
        out.append(f'{name}{{{front},is_streaming="true"}} {round(v * 0.8, 1)}')
        out.append(f'{name}{{{front},is_streaming="false"}} {round(v * 0.2, 1)}')
    out.append("# TYPE sglang:num_requests_total counter")
    out.append(f'sglang:num_requests_total{{{front},is_streaming="true"}} {round(total_reqs * 0.7, 1)}')
    out.append(f'sglang:num_requests_total{{{front},is_streaming="false"}} {round(total_reqs * 0.3, 1)}')
    out.append("# TYPE sglang:num_aborted_requests_total counter")
    out.append(f"sglang:num_aborted_requests_total{{{front}}} {round(total_reqs * 0.015, 1)}")
    out.append("# TYPE sglang:cached_tokens_total counter")
    out.append(
        f'sglang:cached_tokens_total{{cache_source="device",{front}}} '
        f"{round(mono(cfg['prompt_tps'] * cfg['hit_rate'], 17), 1)}"
    )

    # Histograms: cumulative buckets that grow with total_reqs while keeping
    # a fixed per-server shape, so windowed percentile estimates are stable
    # but non-trivial. Frontend histograms split over is_streaming to
    # exercise the bucket-sum merge.
    def histogram(name, buckets, weights, labels, scale=1.0, split=None):
        out.append(f"# TYPE {name} histogram")
        subsets = [(labels, 1.0)] if split is None else split
        total_all = total_reqs * scale
        for lab, share in subsets:
            total = total_all * share
            acc = 0.0
            cum = []
            for w in weights:
                acc += w
                cum.append(acc)
            norm = acc
            for le, c in zip(buckets, cum):
                out.append(f'{name}_bucket{{{lab},le="{le}"}} {round(total * c / norm, 1)}')
            out.append(f'{name}_bucket{{{lab},le="+Inf"}} {round(total, 1)}')
            mid = sum(w * float(b) for w, b in zip(weights, buckets)) / norm
            out.append(f"{name}_sum{{{lab}}} {round(total * mid, 2)}")
            out.append(f"{name}_count{{{lab}}} {round(total, 1)}")

    def streamed(labels):
        return [
            (f'{labels},is_streaming="true"', 0.7),
            (f'{labels},is_streaming="false"', 0.3),
        ]

    ttft_b = ["0.1", "0.2", "0.4", "0.6", "0.8", "1", "2", "4", "6"]
    lat_b = ["0.1", "0.2", "0.4", "0.8", "2", "4", "8", "20", "40"]
    itl_b = ["0.002", "0.006", "0.010", "0.020", "0.040", "0.080", "0.2"]
    histogram(
        "sglang:time_to_first_token_seconds", ttft_b, cfg["ttft_w"], front, split=streamed(front)
    )
    histogram("sglang:inter_token_latency_seconds", itl_b, cfg["itl_w"], front, scale=40)
    histogram(
        "sglang:e2e_request_latency_seconds", lat_b, cfg["e2e_w"], front, split=streamed(front)
    )
    histogram(
        "sglang:queue_time_seconds", lat_b, [30, 40, 20, 6, 2, 1, 0.5, 0.3, 0.2], sched
    )
    out.append("# TYPE sglang:per_stage_req_latency_seconds histogram")
    for stage, weights in (
        ("request_process", [60, 25, 10, 3, 1, 0.5, 0.3, 0.1, 0.1]),
        ("prefill_forward", [10, 30, 35, 15, 6, 2, 1, 0.6, 0.4]),
    ):
        lab = f'{front},stage="{stage}"'
        acc = 0.0
        cum = []
        for w in weights:
            acc += w
            cum.append(acc)
        for le, c in zip(lat_b, cum):
            out.append(
                f'sglang:per_stage_req_latency_seconds_bucket{{{lab},le="{le}"}} '
                f"{round(total_reqs * c / cum[-1], 1)}"
            )
        out.append(
            f'sglang:per_stage_req_latency_seconds_bucket{{{lab},le="+Inf"}} {round(total_reqs, 1)}'
        )
        mid = sum(w * float(b) for w, b in zip(weights, lat_b)) / cum[-1]
        out.append(f"sglang:per_stage_req_latency_seconds_sum{{{lab}}} {round(total_reqs * mid, 2)}")
        out.append(f"sglang:per_stage_req_latency_seconds_count{{{lab}}} {round(total_reqs, 1)}")

    tok_b = ["100", "300", "500", "700", "1000", "2000", "5000", "10000"]
    histogram("sglang:prompt_tokens_histogram", tok_b, [1, 6, 22, 30, 24, 11, 4, 2], front)
    histogram("sglang:generation_tokens_histogram", tok_b, [4, 14, 30, 26, 16, 7, 2.5, 0.5], front)
    return "\n".join(out) + "\n"


def build_loads(cfg):
    s = snapshot(cfg)
    return json.dumps(
        {
            "timestamp": "2026-01-01T00:00:00+00:00",
            "version": cfg["version"],
            "loads": [
                {
                    "dp_rank": 0,
                    "num_running_reqs": s["running"],
                    "num_waiting_reqs": s["waiting"],
                    "num_used_tokens": s["used_tokens"],
                    "max_total_num_tokens": cfg["capacity"],
                    "max_running_requests": cfg["max_running"],
                    "token_usage": s["usage"],
                    "gen_throughput": s["gen_tps_gauge"],
                    "cache_hit_rate": s["hit_rate"],
                    "speculative": {
                        "accept_length": 2.9 if cfg["spec"] else 0.0,
                        "accept_rate": 0.57 if cfg["spec"] else 0.0,
                    },
                    "disaggregation": {"mode": "null"},
                    "queues": {"waiting": s["waiting"], "grammar": 0, "paused": 0, "retracted": 0},
                }
            ],
        }
    )


def build_server_info(cfg):
    return json.dumps(
        {
            "version": cfg["version"],
            "max_running_requests": cfg["max_running"],
            "max_total_num_tokens": cfg["capacity"],
            "speculative_algorithm": "DFLASH" if cfg["spec"] else None,
            "disaggregation_mode": "null",
            "enable_metrics": cfg["metrics"],
            "dp_size": 1,
            "tp_size": 1,
            "served_model_name": cfg["model"],
        }
    )


class Handler(BaseHTTPRequestHandler):
    cfg = None

    def do_GET(self):
        cfg = self.cfg
        if self.path == "/metrics":
            if not cfg["metrics"]:
                self.send_error(404)
                return
            body = build_metrics(cfg).encode()
            ctype = "text/plain; version=0.0.4"
        elif self.path == "/ping":
            # The liveness endpoint sglangtop actually polls (unconditional
            # 200 upstream, no generation).
            body, ctype = b"", "text/plain"
        elif self.path in ("/health", "/health_generate"):
            # Present for realism only. Upstream these share one handler that
            # runs a real one-token generation when
            # SGLANG_ENABLE_HEALTH_ENDPOINT_GENERATION is set, so sglangtop
            # must never call them; a delay makes an accidental probe obvious.
            time.sleep(1.0)
            body, ctype = b"", "text/plain"
        elif self.path in ("/server_info", "/get_server_info"):
            body = build_server_info(cfg).encode()
            ctype = "application/json"
        elif self.path == "/v1/loads":
            body = build_loads(cfg).encode()
            ctype = "application/json"
        elif self.path == "/v1/models":
            body = json.dumps(
                {"object": "list", "data": [{"id": cfg["model"], "object": "model"}]}
            ).encode()
            ctype = "application/json"
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


PROFILES = {
    "a": dict(
        model="example-org/example-model-27B",
        version="0.5.19", metrics=True, spec=True,
        req_rate=0.8, prompt_tps=220.0, gen_tps=48.0,
        run_base=3.2, run_amp=2.6, kv_base=0.42, kv_amp=0.22,
        capacity=131016, max_running=48, hit_rate=0.58, phase=0.0,
        ttft_w=[5, 20, 35, 22, 10, 5, 2, 0.7, 0.3],
        itl_w=[10, 30, 35, 18, 5, 1.5, 0.5],
        e2e_w=[1, 3, 8, 18, 30, 25, 10, 4, 1],
    ),
    "b": dict(
        model="example-org/example-model-7B",
        version="0.5.18", metrics=True, spec=False,
        req_rate=2.2, prompt_tps=520.0, gen_tps=130.0,
        run_base=1.8, run_amp=1.5, kv_base=0.22, kv_amp=0.13,
        capacity=262032, max_running=128, hit_rate=0.71, phase=2.1,
        ttft_w=[15, 35, 30, 12, 5, 2, 0.7, 0.2, 0.1],
        itl_w=[25, 40, 25, 8, 1.5, 0.4, 0.1],
        e2e_w=[4, 10, 22, 30, 20, 9, 3, 1.5, 0.5],
    ),
    # Launched without --enable-metrics: /metrics 404s, /v1/loads carries
    # the gauges. Exercises sglangtop's loads-only fallback + LOADS badge.
    "edge": dict(
        model="example-org/example-model-9B",
        version="0.5.19", metrics=False, spec=True,
        req_rate=0.5, prompt_tps=90.0, gen_tps=25.0,
        run_base=1.2, run_amp=1.1, kv_base=0.15, kv_amp=0.1,
        capacity=65536, max_running=26, hit_rate=0.4, phase=4.0,
        ttft_w=[5, 20, 35, 22, 10, 5, 2, 0.7, 0.3],
        itl_w=[10, 30, 35, 18, 5, 1.5, 0.5],
        e2e_w=[1, 3, 8, 18, 30, 25, 10, 4, 1],
    ),
}

if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--profile", choices=sorted(PROFILES), default="a")
    args = ap.parse_args()
    Handler.cfg = PROFILES[args.profile]
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()

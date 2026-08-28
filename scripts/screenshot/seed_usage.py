#!/usr/bin/env python3
"""Seed a synthetic 30-day usage.db so screenshots show full daily charts.

Creates the recorder's schema (v1) and inserts cumulative counter snapshots
(4/day per endpoint) with a weekly-ish traffic wave. Purely synthetic,
sanitized model names only.
"""

import math
import sqlite3
import sys
import time

path = sys.argv[1]
conn = sqlite3.connect(path)
conn.executescript(
    """
    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS samples (
        ts_ms    INTEGER NOT NULL,
        endpoint TEXT    NOT NULL,
        model    TEXT    NOT NULL,
        engine   TEXT,
        metric   TEXT    NOT NULL,
        value    REAL    NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples (ts_ms);
    CREATE INDEX IF NOT EXISTS idx_samples_ep_metric_ts
        ON samples (endpoint, metric, ts_ms);
    CREATE INDEX IF NOT EXISTS idx_samples_metric_ts
        ON samples (metric, ts_ms);
    INSERT OR IGNORE INTO meta VALUES ('schema_version', '1');
    """
)

now_ms = int(time.time() * 1000)
rows = []
for ep, model, scale in (
    ("local", "example-org/example-model-27B", 1.0),
    ("spark-a", "example-org/example-model-7B", 2.4),
):
    gen = prompt = req = 0.0
    for d in range(30):
        # Weekly-ish wave, always positive; weekends dip.
        wave = 0.75 + 0.45 * math.sin(d / 3.1) + (-0.35 if d % 7 in (5, 6) else 0.0)
        wave = max(0.15, wave)
        for s in range(4):
            ts = now_ms - (29 - d) * 86_400_000 + (2 + 5 * s) * 3_600_000
            if ts > now_ms:
                continue
            gen += scale * wave * 120_000 / 4
            prompt += scale * wave * 480_000 / 4
            req += scale * wave * 950 / 4
            for metric, val in (
                ("generation_tokens_total", gen),
                ("prompt_tokens_total", prompt),
                ("requests_total", req),
            ):
                rows.append((ts, ep, model, "0", metric, round(val, 1)))

conn.executemany("INSERT INTO samples VALUES (?, ?, ?, ?, ?, ?)", rows)
conn.commit()
print(f"seeded {len(rows)} rows into {path}")

# Screenshot pipeline

Regenerates the README screenshots (`docs/img/fleet.svg`,
`docs/img/endpoint.svg`) from **mock** SGLang servers — no GPU, no real
endpoint, no network beyond localhost.

```bash
# inside Linux/WSL, from the repo root
bash scripts/screenshot/screenshot.sh

# longer/shorter history in the charts:
WARMUP=90 bash scripts/screenshot/screenshot.sh
```

Requires `cargo`, `tmux`, `python3` (stdlib only).

## How it works

1. `mock_sglang.py` serves `/metrics` (SGLang 0.5.19-era metric names and
   label shapes with smoothly evolving, monotonic values), `/health`,
   `/server_info`, `/v1/models`, and `/v1/loads` on ports
   18001/18002/18003 — three "servers" with different models and load
   shapes. The third runs the `edge` profile, which simulates a launch
   without `--enable-metrics`: `/metrics` 404s, so the capture exercises
   the `/v1/loads` fallback and its `LOADS` badge. Model names are
   sanitized (`example-org/...`).
2. `seed_usage.py` writes a synthetic 30-day `usage.db` (recorder schema
   v1) so the daily bar charts render full.
3. `screenshot.sh` builds the release binary, runs it in a detached
   120×32 tmux pane (with an explicit `stty rows/cols` — a fresh pane can
   otherwise report 0×0 to the child), lets ~45 s of history accumulate,
   and captures each view with `tmux capture-pane -e`.
4. `ans2svg.py` renders the ANSI capture to SVG: backgrounds as rect runs,
   text as one absolutely-positioned `<text>` per **word** (whole-line
   `textLength` visibly distorts glyph spacing; per-word it does not).

The captures land in `~/sglangtop-shots/*.ans` if you want to re-render
without re-running the TUI.

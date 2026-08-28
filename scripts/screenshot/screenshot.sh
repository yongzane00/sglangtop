#!/usr/bin/env bash
# Regenerate docs/img/{fleet,endpoint}.svg from mock SGLang servers and a
# seeded 30-day usage database.
#
# Run inside Linux/WSL from the repo root:  bash scripts/screenshot/screenshot.sh
# Requires: cargo, tmux, python3. Never touches a real SGLang server.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$REPO/scripts/screenshot"
export PATH="$HOME/.cargo/bin:$PATH" # non-login shells (wsl -e) miss it
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/build/sglangtop-target}"
WORK="$HOME/sglangtop-shots"
SESSION=sgltshot
COLS=120
ROWS=32
WARMUP="${WARMUP:-45}" # seconds of history before capturing

mkdir -p "$WORK"
cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  [ -n "${PID_A:-}" ] && kill "$PID_A" 2>/dev/null || true
  [ -n "${PID_B:-}" ] && kill "$PID_B" 2>/dev/null || true
  [ -n "${PID_C:-}" ] && kill "$PID_C" 2>/dev/null || true
}
trap cleanup EXIT

echo "== build release =="
(cd "$REPO" && cargo build --release)
BIN="$CARGO_TARGET_DIR/release/sglangtop"

echo "== seed 30-day usage database =="
rm -f "$WORK/usage.db" "$WORK/usage.db-wal" "$WORK/usage.db-shm"
python3 "$HERE/seed_usage.py" "$WORK/usage.db"

echo "== start mock servers on 18001/18002/18003 =="
python3 "$HERE/mock_sglang.py" --port 18001 --profile a &
PID_A=$!
python3 "$HERE/mock_sglang.py" --port 18002 --profile b &
PID_B=$!
# Profile "edge" has --enable-metrics off: exercises the /v1/loads fallback.
python3 "$HERE/mock_sglang.py" --port 18003 --profile edge &
PID_C=$!
sleep 1
curl -fsS http://127.0.0.1:18001/metrics >/dev/null
curl -fsS http://127.0.0.1:18002/metrics >/dev/null
curl -fsS http://127.0.0.1:18003/v1/loads >/dev/null

echo "== write shoot config =="
cat >"$WORK/shoot.toml" <<EOF
refresh_interval_ms = 1000
record_path = "$WORK/usage.db"

[[endpoints]]
name = "local"
url = "http://127.0.0.1:18001"

[[endpoints]]
name = "spark-a"
url = "http://127.0.0.1:18002"

[[endpoints]]
name = "edge"
url = "http://127.0.0.1:18003"
EOF

echo "== run sglangtop in a ${COLS}x${ROWS} tmux pane =="
tmux kill-session -t "$SESSION" 2>/dev/null || true
tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS"
# A fresh detached pane can report 0x0 to the child; pin the size first.
tmux send-keys -t "$SESSION" \
  "stty rows $ROWS cols $COLS; $BIN --config $WORK/shoot.toml" Enter

echo "== warm up ${WARMUP}s of history =="
sleep "$WARMUP"

echo "== capture fleet view =="
tmux send-keys -t "$SESSION" 1
sleep 2
tmux capture-pane -t "$SESSION" -e -p >"$WORK/fleet.ans"

echo "== capture endpoint view (charts + latency tables) =="
tmux send-keys -t "$SESSION" 2
sleep 2
tmux capture-pane -t "$SESSION" -e -p >"$WORK/endpoint.ans"

tmux send-keys -t "$SESSION" q
sleep 1

echo "== render SVGs =="
mkdir -p "$REPO/docs/img"
python3 "$HERE/ans2svg.py" "$WORK/fleet.ans" "$REPO/docs/img/fleet.svg"
python3 "$HERE/ans2svg.py" "$WORK/endpoint.ans" "$REPO/docs/img/endpoint.svg"
echo "wrote docs/img/fleet.svg and docs/img/endpoint.svg"

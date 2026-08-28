# Development environment

sglangtop targets Linux, but this repo has been developed from a Windows 11
host with the checkout on the Windows filesystem and all builds inside
WSL2 Ubuntu. These are the practices and traps that made that workable.

## Building (Windows host + WSL2)

- No Rust toolchain on Windows; everything runs in WSL Ubuntu (rustup
  stable + `build-essential` for the bundled SQLite's C compiler).
- **Always set `CARGO_TARGET_DIR` to a Linux-side path**
  (e.g. `$HOME/build/sglangtop-target`). The checkout under `/mnt/c/...` is
  served over 9P; leaving the target dir there makes builds crawl by an
  order of magnitude. Source reads over 9P are tolerable; target-dir I/O is
  not.
- WSL specifics that bite scripted workflows:
  - `/tmp` is tmpfs **and** the WSL VM can restart between `wsl.exe`
    invocations — anything needed across invocations goes in `$HOME` (or
    the Windows side).
  - Non-login shells (`wsl -e bash`) don't source `~/.profile`; export
    `PATH="$HOME/.cargo/bin:$PATH"` explicitly in scripts.
  - Inline quoting through `wsl.exe` mangles complex commands, and
    PowerShell expands `$HOME`/`$VAR` inside double quotes *before* WSL
    sees them. The reliable pattern: write a `.sh` file (LF endings) and
    run `wsl -d Ubuntu -e bash /mnt/c/path/to/script.sh`.
  - When PowerShell must read repo files (e.g. grepping generated SVGs),
    use `[IO.File]::ReadAllText($path, [Text.Encoding]::UTF8)` — default
    encoding handling mangles UTF-8 box-drawing/braille glyphs.

## Verification

All four must pass before a change is done (CI mirrors them):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo build --release
```

Conventions the suite depends on:

- Tests never touch the network (`httpmock` for HTTP, `tempfile` for
  SQLite/log files).
- `std::env::set_var` is unsafe in edition 2024 and banned; env access is
  injected as closures (`get_env: impl Fn(&str) -> Option<String>`) so
  tests stub it without process-global state.
- Tailer tests drive `poll_step()` directly instead of sleeping — no
  timing-dependent assertions.
- Date-sensitive tests ask SQLite (`strftime(...,'localtime')`) for
  expected day strings, so they pass in any timezone.

## Screenshot pipeline (`scripts/screenshot/`)

`bash scripts/screenshot/screenshot.sh` (inside WSL, from the repo root)
regenerates `docs/img/{fleet,endpoint}.svg` with **no real SGLang server**:

1. `mock_sglang.py` — stdlib-only mock servers on 18001/18002/18003 with
   smoothly evolving, monotonic metrics (sanitized `example-org/...` model
   names). The third server runs the `edge` profile, which simulates
   `--enable-metrics` off: `/metrics` 404s so the capture also exercises
   the `/v1/loads` fallback and its `LOADS` badge.
2. `seed_usage.py` — a synthetic 30-day `usage.db` (recorder schema v1)
   so the daily bar charts render full.
3. The release binary runs in a detached 120×32 tmux pane. **A fresh
   detached pane can report a 0×0 pty to its child** — `stty rows/cols`
   inside the pane first, or captures come out blank.
4. `tmux capture-pane -e` → `ans2svg.py`, which renders backgrounds as rect
   runs and text as one absolutely-positioned `<text>` per **word**
   (whole-line `textLength` visibly distorts glyph spacing; per-word does
   not — SVG renderers collapse run whitespace, so spaces are never drawn
   as glyphs).

Captured `.ans` files persist in `~/sglangtop-shots/` for re-rendering
without re-running the TUI.

## Live validation protocol

- A private development SGLang endpoint may exist in the local environment.
  **Its address must never appear in the repo, fixtures, screenshots, or
  docs** — shell commands and WSL-side files only.
- The endpoint is sometimes unreachable (powered off). `curl` its
  `/metrics` first; **never claim live validation without an actual
  successful run**. When it is down, mock-based end-to-end runs plus
  failure-mode observations (INIT states, `--` axes, empty-state hints) are
  reported as exactly that — not as live validation.
- Live runs are read-only GETs by design; never send inference requests to
  the dev endpoint.

## Repo hygiene

- Fixtures are sanitized: no private hosts/IPs/model deployments; model
  names use `example-org/...`; invented prompts only.
- Before anything leaves this machine: sweep for private addresses and any
  non-example hostnames across `docs/`, `tests/fixtures/`, and
  `docs/img/*.svg`.
- README.md is a public landing page — internal status and open decisions
  belong in [PLAN.md](PLAN.md), never there (see the `public-readme`
  project skill in `.claude/skills/`).

//! Render tests: draw real frames into a `TestBackend` buffer.
//!
//! Every `draw*` function in `ui/` was previously unreachable from the test
//! suite — a layout panic or a constraint overflow could only be found by
//! running the binary. These cover the whole matrix of sizes, views and
//! panel modes, and assert the honesty rules that are easy to regress
//! silently (`--` never becoming `0`, stale data staying visible).

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

use crate::app::testing::{app_with, app_with_fixture};
use crate::app::{App, PanelMode, View};
use crate::ui::theme::Theme;

/// Sizes from "generous desktop" down to absurd. Nothing may panic at any of
/// them; the small ones exist purely to prove the ladder bottoms out safely.
const SIZES: [(u16, u16); 10] = [
    (200, 50),
    (160, 44),
    // Portrait: a terminal on a vertical monitor, full and half height.
    (110, 95),
    (110, 44),
    (90, 60),
    (120, 30),
    (100, 24),
    (80, 20),
    (40, 10),
    (20, 5),
];

fn render(app: &App, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| crate::ui::draw(f, app)).unwrap();
    term.backend().buffer().clone()
}

fn text(buf: &Buffer) -> String {
    let area = *buf.area();
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn fleet_renders_at_every_size_in_both_themes() {
    for theme in [Theme::mono(), Theme::truecolor()] {
        let mut app = app_with_fixture(&["http://h1:1", "http://h2:2"]);
        app.theme = theme;
        for (w, h) in SIZES {
            let out = text(&render(&app, w, h));
            if w >= 100 {
                assert!(out.contains("sglangtop"), "{w}x{h}: {out}");
                assert!(out.contains("1:ALL"), "{w}x{h}: {out}");
            }
        }
    }
}

#[test]
fn endpoint_renders_in_every_panel_mode_at_every_size() {
    let mut app = app_with_fixture(&["http://h1:1", "http://h2:2"]);
    app.view = View::Endpoint(0);
    for mode in [PanelMode::Overview, PanelMode::Requests, PanelMode::Tables] {
        app.panel_mode = mode;
        for (w, h) in SIZES {
            let out = text(&render(&app, w, h));
            if w >= 60 && h >= 24 {
                assert!(out.contains("SEATS"), "{mode:?} {w}x{h}: {out}");
                assert!(out.contains("KV"), "{mode:?} {w}x{h}: {out}");
                assert!(out.contains(mode.label()), "{mode:?} {w}x{h}: {out}");
            }
        }
    }
}

#[test]
fn overview_shows_every_instrument_wide_and_in_portrait() {
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.view = View::Endpoint(0);
    for (w, h) in [(200, 50), (110, 44), (110, 95)] {
        let out = text(&render(&app, w, h));
        for expected in [
            "KV CACHE",
            "SEATS",
            "tokens/s",
            "out",
            "TTFT p50",
            "TTFT p95",
            "percentiles",
            "server & model",
        ] {
            assert!(out.contains(expected), "{w}x{h} missing {expected}:\n{out}");
        }
    }
    // A tall portrait terminal has room for the events feed too.
    let out = text(&render(&app, 110, 95));
    assert!(out.contains("events"), "{out}");
}

#[test]
fn a_small_terminal_gets_one_line_gauges() {
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.view = View::Endpoint(0);
    let out = text(&render(&app, 80, 22));
    for expected in ["KV ", "SEATS ", "TTFT ", "tokens/s"] {
        assert!(out.contains(expected), "missing {expected}:\n{out}");
    }
    assert!(!out.contains("KV CACHE"), "{out}");
}

#[test]
fn the_tokens_panel_lists_prompts_as_a_ledger() {
    // The fixture's second scrape bumps the prompt-token counter by 10%
    // (7.3k tokens) and the request counter by half a request, which rounds
    // to one: a single-request ledger row with its share of the 262k
    // context window and a window total, under the generation band.
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.view = View::Endpoint(0);
    for (w, h) in [(110, 44), (110, 95), (200, 50)] {
        let out = text(&render(&app, w, h));
        for expected in ["out", "in", "Σ", "7.3k", "3% ctx"] {
            assert!(out.contains(expected), "{w}x{h} missing {expected}:\n{out}");
        }
        assert!(!out.contains("no prompts"), "{out}");
        // Half a request rounds to one, so the row names no count.
        assert!(!out.contains("reqs"), "{out}");
    }
}

#[test]
fn the_removed_charts_stay_removed() {
    // KV is a bar now; inter-token latency and completions/s live in the
    // percentiles table and the fleet grid, not the endpoint overview.
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.view = View::Endpoint(0);
    for (w, h) in [(200, 50), (110, 95)] {
        let out = text(&render(&app, w, h));
        assert!(!out.contains("completions/s"), "{out}");
        assert!(!out.contains("inter-token p95"), "{out}");
    }
}

#[test]
fn a_declared_cap_fills_the_seat_grid() {
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.config.endpoints[0].max_running = Some(12);
    app.theme = Theme::truecolor();
    app.view = View::Endpoint(0);
    let out = text(&render(&app, 110, 44));
    assert!(out.contains("/ 12 running"), "{out}");
    assert!(out.contains('\u{25a1}'), "{out}");
    // Without one, the title asks for it and no empty seat is drawn.
    app.config.endpoints[0].max_running = None;
    let out = text(&render(&app, 110, 44));
    assert!(out.contains("cap ? (add @N)"), "{out}");
    assert!(!out.contains('\u{25a1}'), "{out}");
}

#[test]
fn an_endpoint_with_no_data_shows_dashes_and_never_a_fabricated_zero() {
    let mut app = app_with(&["http://h1:1"]);
    app.view = View::Endpoint(0);
    let out = text(&render(&app, 160, 44));
    assert!(out.contains("--"), "{out}");
    // The KV card must not claim a measured 0% when nothing was scraped, and
    // STATUS must not claim IDLE when no running gauge was ever seen — which
    // is exactly what a non-vLLM backend (a vLLM server, whose metrics are all
    // named `vllm:*`) looks like to us.
    assert!(!out.contains("0.0%"), "{out}");
    assert!(!out.contains("IDLE"), "{out}");
}

#[test]
fn help_overlay_renders_over_both_views() {
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.show_help = true;
    for view in [View::Fleet, View::Endpoint(0)] {
        app.view = view;
        for (w, h) in [(200u16, 50u16), (80, 20), (20, 5)] {
            let out = text(&render(&app, w, h));
            if w >= 80 && h >= 20 {
                assert!(out.contains("quit"), "{view:?} {w}x{h}: {out}");
            }
        }
    }
}

#[test]
fn zero_endpoints_renders_without_panicking() {
    let mut app = app_with(&["http://h1:1"]);
    app.endpoints.clear();
    for view in [View::Fleet, View::Endpoint(0)] {
        app.view = view;
        for (w, h) in [(200u16, 50u16), (120, 30), (20, 5)] {
            let _ = render(&app, w, h);
        }
    }
}

#[test]
fn out_of_range_endpoint_tab_renders_nothing_rather_than_panicking() {
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.view = View::Endpoint(99);
    let _ = render(&app, 160, 44);
}

#[test]
fn usage_chart_with_an_empty_day_list_does_not_panic() {
    // Guards the underflow in `fleet::draw_usage_chart`: every index there
    // derives from `fit - 1`, which wraps when no days are recorded.
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.usage.disabled = None;
    app.usage.data = Some(crate::storage::usage::DailyUsage {
        days: Vec::new(),
        queried_at_ms: 0,
    });
    let out = text(&render(&app, 200, 50));
    assert!(out.contains("no usage recorded yet"), "{out}");
}

#[test]
fn usage_chart_with_a_single_day_renders() {
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.usage.disabled = None;
    app.usage.data = Some(crate::storage::usage::DailyUsage {
        days: vec![crate::storage::usage::DayUsage {
            day: "2026-09-18".into(),
            prompt_tokens: Some(1000.0),
            generation_tokens: Some(250.0),
            requests: Some(12.0),
        }],
        queried_at_ms: 0,
    });
    let out = text(&render(&app, 200, 50));
    assert!(out.contains("output tokens/day"), "{out}");
}

#[test]
fn a_multi_engine_endpoint_still_renders_every_panel_mode() {
    // The fixture is single-engine; duplicate its series under a second
    // engine label to exercise the multi-series paths (per-key chart lines,
    // "worst across series" percentiles, suppressed sparklines).
    const FIXTURE: &str = include_str!("../../tests/fixtures/sglang_unified_single.txt");
    let doubled: String = FIXTURE
        .lines()
        .flat_map(|line| {
            if line.starts_with('#') || !line.contains("engine=\"0\"") {
                vec![line.to_string()]
            } else {
                vec![
                    line.to_string(),
                    line.replace("engine=\"0\"", "engine=\"1\""),
                ]
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut app = app_with(&["http://h1:1"]);
    app.handle_event(crate::event::AppEvent::Scrape {
        endpoint: 0,
        outcome: crate::state::ScrapeOutcome {
            at: std::time::Instant::now(),
            wall: std::time::SystemTime::now(),
            duration: std::time::Duration::from_millis(10),
            result: Ok(crate::state::ScrapePayload {
                metrics: Some(crate::metrics::parse::parse_text(&doubled)),
                ..Default::default()
            }),
        },
    });
    app.view = View::Endpoint(0);
    for mode in [PanelMode::Overview, PanelMode::Requests, PanelMode::Tables] {
        app.panel_mode = mode;
        let _ = render(&app, 200, 50);
        let _ = render(&app, 120, 30);
    }
}

#[test]
fn mono_and_color_themes_agree_on_layout() {
    // Styles may differ; the glyph grid must not, or an ASCII/Unicode width
    // divergence is lurking.
    let mut app = app_with_fixture(&["http://h1:1"]);
    app.view = View::Endpoint(0);
    app.theme = Theme::mono();
    let mono = text(&render(&app, 160, 44));
    app.theme = Theme::truecolor();
    let color = text(&render(&app, 160, 44));
    for (m, c) in mono.lines().zip(color.lines()) {
        assert_eq!(
            m.chars().count(),
            c.chars().count(),
            "row width differs between themes:\n{m}\n{c}"
        );
    }
}

#[test]
fn renders_at_every_layout_boundary_without_panicking() {
    // Sizes clustered around every threshold in the ladders (card columns at
    // 36/100/150, chart columns at 100/150, detail band at 110/150, and the
    // minimum heights) — off-by-ones live exactly here, not in round numbers.
    const WIDTHS: [u16; 19] = [
        19, 20, 21, 35, 36, 37, 55, 56, 57, 99, 100, 101, 109, 110, 111, 149, 150, 151, 200,
    ];
    const HEIGHTS: [u16; 16] = [3, 4, 5, 6, 11, 12, 13, 19, 20, 21, 29, 30, 31, 43, 44, 50];

    let mut app = app_with_fixture(&["http://h1:1", "http://h2:2"]);
    for &w in &WIDTHS {
        for &h in &HEIGHTS {
            app.view = View::Fleet;
            let _ = render(&app, w, h);
            app.view = View::Endpoint(0);
            for mode in [PanelMode::Overview, PanelMode::Requests, PanelMode::Tables] {
                app.panel_mode = mode;
                let _ = render(&app, w, h);
            }
        }
    }
}

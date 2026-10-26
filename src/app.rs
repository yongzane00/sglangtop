//! The main application: owns all state, reduces events, routes input,
//! and drives rendering. Runs entirely on one task — collectors and the
//! recorder communicate exclusively through channels.

use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use tokio::sync::mpsc;

use crate::collector::CollectorControl;
use crate::config::{Config, MAX_REFRESH_MS, MIN_REFRESH_MS};
use crate::event::AppEvent;
use crate::state::EndpointState;
use crate::storage::{Recorder, SampleRow, now_ms};
use crate::ui::theme::Theme;

/// Which screen is showing. Ordering for Tab cycling:
/// Fleet → Endpoint(0..n) → Fleet…
///
/// The fleet view embeds the rolling history charts below the endpoint
/// table (PgUp/PgDn scrolls them); there is no separate History or Raw view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Fleet,
    Endpoint(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetSort {
    Name,
    Running,
    Waiting,
    GenTps,
}

impl FleetSort {
    pub fn next(self) -> Self {
        match self {
            FleetSort::Name => FleetSort::Running,
            FleetSort::Running => FleetSort::Waiting,
            FleetSort::Waiting => FleetSort::GenTps,
            FleetSort::GenTps => FleetSort::Name,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            FleetSort::Name => "name",
            FleetSort::Running => "running",
            FleetSort::Waiting => "waiting",
            FleetSort::GenTps => "gen tokens/s",
        }
    }
}

/// Record cumulative counter snapshots at most this often (per endpoint).
/// One cadence bounds the error at day boundaries and restarts; 30 s keeps
/// the usage rows ~0.3% of the recorder's volume.
const COUNTER_RECORD_EVERY: Duration = Duration::from_secs(30);
/// Re-run the daily-usage query this often (plus on startup and on `r`).
const USAGE_REFRESH_EVERY: Duration = Duration::from_secs(300);

/// Cached result of the daily-usage query, plus why it may be unavailable.
#[derive(Debug, Default)]
pub struct UsageCache {
    /// Last good result; kept across later query failures.
    pub data: Option<crate::storage::usage::DailyUsage>,
    pub last_error: Option<String>,
    /// Recording is off/broken, with the display-safe reason.
    pub disabled: Option<String>,
}

/// What the endpoint view shows BELOW its card band. `t` cycles it.
///
/// Deliberately one global setting rather than one per endpoint: it is a
/// lens preference ("show me the latency tables"), not a property of a
/// machine. With `Tab` cycling a dozen endpoints, a per-endpoint mode would
/// change the screen's shape as you move between them — the worst property
/// for a glance tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelMode {
    /// The six live charts.
    #[default]
    Overview,
    /// Token-rate charts beside the live request log.
    Requests,
    /// The ACTIVITY / LATENCY percentile tables.
    Tables,
}

impl PanelMode {
    pub fn next(self) -> Self {
        match self {
            PanelMode::Overview => PanelMode::Requests,
            PanelMode::Requests => PanelMode::Tables,
            PanelMode::Tables => PanelMode::Overview,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PanelMode::Overview => "overview",
            PanelMode::Requests => "requests",
            PanelMode::Tables => "tables",
        }
    }

    /// What pressing `t` gives you next — what the footer advertises.
    pub fn next_label(self) -> &'static str {
        self.next().label()
    }
}

pub struct App {
    pub config: Config,
    pub theme: Theme,
    pub endpoints: Vec<EndpointState>,
    pub view: View,
    pub fleet_selected: usize,
    pub fleet_sort: FleetSort,
    /// Scroll offset (in chart-grid rows) of the charts embedded in the
    /// fleet view; the renderer clamps it to the real row count.
    pub fleet_chart_scroll: usize,
    pub paused: bool,
    pub show_help: bool,
    /// What the endpoint view shows below its cards ('t' cycles).
    pub panel_mode: PanelMode,
    /// Runtime-adjustable; starts at config.refresh_interval.
    pub refresh_interval: Duration,
    pub recorder: Option<Recorder>,
    /// Recording failed to start (shown in the header; app keeps running).
    pub recorder_error: Option<String>,
    /// Fleet daily-usage charts read this, never the database directly.
    pub usage: UsageCache,
    usage_query_in_flight: bool,
    usage_last_query: Option<Instant>,
    /// Per-endpoint last cumulative-counter snapshot (throttle state).
    counter_recorded_at: Vec<Option<Instant>>,
    /// For handing results of off-loop work back into the reducer.
    events_tx: mpsc::Sender<AppEvent>,
    control: CollectorControl,
    should_quit: bool,
    dirty: bool,
}

impl App {
    pub fn new(
        config: Config,
        theme: Theme,
        control: CollectorControl,
        events_tx: mpsc::Sender<AppEvent>,
    ) -> App {
        let endpoints: Vec<EndpointState> = config
            .endpoints
            .iter()
            .map(|e| {
                EndpointState::new(
                    e.name.clone(),
                    e.display_url(),
                    config.history_window,
                    config.percentile_window,
                )
            })
            .collect();
        let (recorder, recorder_error) = match config.record.path() {
            Some(path) => match Recorder::start(path, config.retention_days) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(e)),
            },
            None => (None, None),
        };
        // The daily-usage charts must say WHY they are empty, not show zeros.
        let usage_disabled = match &config.record {
            crate::config::RecordSetting::Enabled(_) => recorder_error.clone(),
            crate::config::RecordSetting::DisabledByUser => {
                Some("recording disabled (--no-record)".into())
            }
            crate::config::RecordSetting::Unavailable(reason) => Some(reason.clone()),
        };
        let n = endpoints.len();
        App {
            refresh_interval: config.refresh_interval,
            theme,
            endpoints,
            view: View::Fleet,
            fleet_selected: 0,
            fleet_sort: FleetSort::Name,
            fleet_chart_scroll: 0,
            paused: false,
            show_help: false,
            panel_mode: PanelMode::default(),
            recorder,
            recorder_error,
            usage: UsageCache {
                data: None,
                last_error: None,
                disabled: usage_disabled,
            },
            usage_query_in_flight: false,
            usage_last_query: None,
            counter_recorded_at: vec![None; n],
            events_tx,
            control,
            should_quit: false,
            dirty: true,
            config,
        }
    }

    /// Main loop: reduce events, redraw when dirty (throttled), exit on
    /// quit keys or SIGTERM. Never busy-loops: it always parks on the
    /// channel/tick select.
    pub async fn run(
        mut self,
        terminal: &mut ratatui::DefaultTerminal,
        mut events: mpsc::Receiver<AppEvent>,
    ) -> anyhow::Result<()> {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Backdated so the first frame paints immediately instead of waiting
        // for the first tick.
        let mut min_frame = Instant::now()
            .checked_sub(Duration::from_millis(200))
            .unwrap_or_else(Instant::now);

        #[cfg(unix)]
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        loop {
            if self.dirty && min_frame.elapsed() >= Duration::from_millis(90) {
                terminal.draw(|frame| crate::ui::draw(frame, &self))?;
                self.dirty = false;
                min_frame = Instant::now();
            }
            #[cfg(unix)]
            let term_signal = sigterm.recv();
            #[cfg(not(unix))]
            let term_signal = std::future::pending::<Option<()>>();

            tokio::select! {
                maybe = events.recv() => match maybe {
                    Some(event) => self.handle_event(event),
                    None => break, // all senders gone; nothing left to show
                },
                _ = tick.tick() => {
                    // Ages/staleness advance even without data; skip while
                    // paused to honor "no automatic refresh".
                    if !self.paused {
                        self.dirty = true;
                    }
                    // Usage refresh runs even while paused: pause freezes
                    // the display, never collection.
                    self.maybe_spawn_usage_query();
                }
                _ = term_signal => break,
            }
            if self.should_quit {
                break;
            }
        }
        if let Some(recorder) = self.recorder.take() {
            recorder.shutdown();
        }
        Ok(())
    }

    pub fn handle_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::MetricsStarted { endpoint, at } => {
                let Some(state) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                state.mark_attempt_started(at);
                if !self.paused {
                    self.dirty = true;
                }
            }
            AppEvent::OptionalUpdate {
                endpoint,
                healthy,
                server_info,
                models,
            } => {
                let Some(state) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                state.apply_optional(healthy, server_info, models);
                if !self.paused {
                    self.dirty = true;
                }
            }
            AppEvent::Scrape { endpoint, outcome } => {
                let Some(state) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                let succeeded = outcome.result.is_ok();
                state.apply(outcome);
                if succeeded {
                    self.record_samples(endpoint);
                }
                // State always updates; pausing only freezes the display.
                if !self.paused {
                    self.dirty = true;
                }
            }
            AppEvent::Key(key) => {
                self.handle_key(key);
                self.dirty = true;
            }
            AppEvent::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_charts(-1),
                    MouseEventKind::ScrollDown => self.scroll_charts(1),
                    _ => {}
                }
                self.dirty = true;
            }
            AppEvent::RequestLog {
                endpoint,
                at,
                status,
                events,
            } => {
                let Some(state) = self.endpoints.get_mut(endpoint) else {
                    return;
                };
                state.requests.apply(at, status, events);
                if !self.paused {
                    self.dirty = true;
                }
            }
            AppEvent::UsageLoaded(result) => {
                self.usage_query_in_flight = false;
                match result {
                    Ok(data) => {
                        self.usage.data = Some(data);
                        self.usage.last_error = None;
                    }
                    // Keep the last good data; surface the error alongside.
                    Err(e) => self.usage.last_error = Some(e),
                }
                if !self.paused {
                    self.dirty = true;
                }
            }
            AppEvent::Resize => self.dirty = true,
            AppEvent::InputClosed => self.should_quit = true,
        }
    }

    fn record_samples(&mut self, endpoint: usize) {
        if self.recorder.is_none() {
            return;
        }
        let now = Instant::now();
        let cumulative_due = self.counter_record_due(endpoint, now);
        if cumulative_due && let Some(slot) = self.counter_recorded_at.get_mut(endpoint) {
            *slot = Some(now);
        }
        let state = &self.endpoints[endpoint];
        let ts_ms = now_ms();
        let to_row = |(key, metric, value): (crate::metrics::normalize::SeriesKey, &str, f64)| {
            let engine = key.engine_str();
            SampleRow {
                ts_ms,
                endpoint: state.name.clone(),
                model: key.model,
                engine,
                metric: metric.to_string(),
                value,
            }
        };
        let mut rows: Vec<SampleRow> = state.current_samples().into_iter().map(to_row).collect();
        if cumulative_due {
            rows.extend(state.cumulative_samples().into_iter().map(to_row));
        }
        if let Some(recorder) = &self.recorder {
            recorder.record(rows);
        }
    }

    /// Cumulative counters are snapshotted at most every
    /// [`COUNTER_RECORD_EVERY`]; the first successful scrape records
    /// immediately.
    fn counter_record_due(&self, endpoint: usize, now: Instant) -> bool {
        match self.counter_recorded_at.get(endpoint).copied().flatten() {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= COUNTER_RECORD_EVERY,
        }
    }

    fn usage_query_due(&self, now: Instant) -> bool {
        if self.recorder.is_none() || self.usage_query_in_flight {
            return false;
        }
        match self.usage_last_query {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= USAGE_REFRESH_EVERY,
        }
    }

    /// Kick the blocking daily-usage query onto a worker thread; the result
    /// comes back through the event channel. At most one in flight.
    fn maybe_spawn_usage_query(&mut self) {
        let now = Instant::now();
        if !self.usage_query_due(now) {
            return;
        }
        let Some(recorder) = &self.recorder else {
            return;
        };
        let path = recorder.path().to_path_buf();
        let tx = self.events_tx.clone();
        self.usage_query_in_flight = true;
        self.usage_last_query = Some(now);
        tokio::task::spawn_blocking(move || {
            let result = crate::storage::usage::query_daily_usage(&path);
            // Send failure just means the app is shutting down.
            let _ = tx.blocking_send(AppEvent::UsageLoaded(result));
        });
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }
        if self.show_help {
            // Any of the dismissal keys closes the overlay.
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter
            ) {
                self.show_help = false;
            }
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Tab => self.view = self.next_view(self.view),
            KeyCode::BackTab => self.view = self.prev_view(self.view),
            KeyCode::Char('1') => self.view = View::Fleet,
            KeyCode::Char(c @ '2'..='9') => {
                let idx = (c as usize) - ('2' as usize);
                if idx < self.endpoints.len() {
                    self.view = View::Endpoint(idx);
                }
            }
            KeyCode::Char('r') => {
                self.control.force_refresh();
                // Also refresh the daily-usage charts right away.
                self.usage_last_query = None;
                self.maybe_spawn_usage_query();
            }
            KeyCode::Char('p') => self.paused = !self.paused,
            KeyCode::Char('t') => {
                if matches!(self.view, View::Endpoint(_)) {
                    self.panel_mode = self.panel_mode.next();
                }
            }
            // Tab aliases, matching the arrow keys people expect in a tabbed
            // TUI. `j`/`k`/Up/Down keep row selection.
            KeyCode::Right => self.view = self.next_view(self.view),
            KeyCode::Left => self.view = self.prev_view(self.view),
            // '+' = faster refresh = SHORTER interval (matches README).
            KeyCode::Char('+') | KeyCode::Char('=') => self.adjust_interval(-1),
            KeyCode::Char('-') => self.adjust_interval(1),
            KeyCode::Char('s') => {
                if self.view == View::Fleet {
                    self.fleet_sort = self.fleet_sort.next();
                }
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            // The chart grid below the fleet table scrolls independently of
            // the row selection.
            KeyCode::PageDown => self.scroll_charts(1),
            KeyCode::PageUp => self.scroll_charts(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                self.fleet_selected = 0;
                self.fleet_chart_scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => self.move_selection(i64::MAX / 2),
            KeyCode::Enter if self.view == View::Fleet => {
                let idx = self
                    .sorted_endpoint_indices()
                    .get(self.fleet_selected)
                    .copied();
                if let Some(idx) = idx {
                    self.view = View::Endpoint(idx);
                }
            }
            _ => {}
        }
    }

    fn adjust_interval(&mut self, direction: i64) {
        let current_ms = self.refresh_interval.as_millis() as i64;
        // Steps: 250ms below 1s, 500ms up to 5s, 1s beyond.
        let step = if current_ms < 1000 {
            250
        } else if current_ms < 5000 {
            500
        } else {
            1000
        };
        let next =
            (current_ms + direction * step).clamp(MIN_REFRESH_MS as i64, MAX_REFRESH_MS as i64);
        self.refresh_interval = Duration::from_millis(next as u64);
        self.control.set_interval(self.refresh_interval);
    }

    /// Move the fleet-table row selection (i128: G's huge jump plus a large
    /// value must clamp, never overflow).
    fn move_selection(&mut self, delta: i64) {
        if self.view != View::Fleet {
            return;
        }
        let max = self.endpoints.len().saturating_sub(1);
        let next = (self.fleet_selected as i128 + i128::from(delta)).clamp(0, max as i128);
        self.fleet_selected = next as usize;
    }

    /// Scroll the chart grid embedded in the fleet view. The bound here only
    /// needs to cover the 1-column layout; the renderer clamps to the real
    /// row count.
    fn scroll_charts(&mut self, delta: i64) {
        if self.view != View::Fleet {
            return;
        }
        let next = (self.fleet_chart_scroll as i128 + i128::from(delta)).clamp(0, 16);
        self.fleet_chart_scroll = next as usize;
    }

    pub fn next_view(&self, view: View) -> View {
        let n = self.endpoints.len();
        match view {
            View::Fleet => {
                if n > 0 {
                    View::Endpoint(0)
                } else {
                    View::Fleet
                }
            }
            View::Endpoint(i) => {
                if i + 1 < n {
                    View::Endpoint(i + 1)
                } else {
                    View::Fleet
                }
            }
        }
    }

    pub fn prev_view(&self, view: View) -> View {
        let n = self.endpoints.len();
        match view {
            View::Fleet => {
                if n > 0 {
                    View::Endpoint(n - 1)
                } else {
                    View::Fleet
                }
            }
            View::Endpoint(0) => View::Fleet,
            View::Endpoint(i) => View::Endpoint(i - 1),
        }
    }

    /// Endpoint indices in fleet-table order under the active sort.
    /// Sorting is deterministic: ties fall back to name.
    pub fn sorted_endpoint_indices(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.endpoints.len()).collect();
        let sort = self.fleet_sort;
        idx.sort_by(|&a, &b| {
            let (ea, eb) = (&self.endpoints[a], &self.endpoints[b]);
            let name = ea.name.cmp(&eb.name);
            let by = |v: Option<f64>| v.unwrap_or(f64::NEG_INFINITY);
            match sort {
                FleetSort::Name => name,
                FleetSort::Running => by(eb.aggregate().running)
                    .partial_cmp(&by(ea.aggregate().running))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(name),
                FleetSort::Waiting => by(eb.aggregate().waiting)
                    .partial_cmp(&by(ea.aggregate().waiting))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(name),
                FleetSort::GenTps => by(eb.aggregate().generation_tps)
                    .partial_cmp(&by(ea.aggregate().generation_tps))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(name),
            }
        });
        idx
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }
}

/// Shared test fixtures. `cfg(test)` only, so nothing ships in the binary.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// `n` endpoints, mono theme, no recorder.
    pub(crate) fn app_with(endpoint_urls: &[&str]) -> App {
        use clap::Parser;
        let mut args = vec!["vllmtop".to_string()];
        for (i, u) in endpoint_urls.iter().enumerate() {
            args.push("--endpoint".into());
            args.push(format!("ep{i}={u}"));
        }
        let cli = crate::cli::Cli::parse_from(args);
        let config: Config = crate::config::load(&cli, |_| None).unwrap();
        let (interval_tx, _rx) = tokio::sync::watch::channel(config.refresh_interval);
        let (force_tx, _rx2) = tokio::sync::watch::channel(0);
        let control = CollectorControl {
            interval_tx,
            force_tx,
        };
        // Tests never spawn the usage query (no recorder without env), so a
        // small dangling channel is fine.
        let (events_tx, _events_rx) = mpsc::channel(8);
        App::new(config, Theme::mono(), control, events_tx)
    }

    /// Two scrapes 2 s apart of the real (sanitized) vLLM capture, so rates,
    /// percentile windows and history rings all hold real values. The second
    /// scrape advances the counters, which is what makes rates non-`None`.
    pub(crate) fn app_with_fixture(endpoint_urls: &[&str]) -> App {
        const FIXTURE: &str = include_str!("../tests/fixtures/sglang_unified_single.txt");
        let mut app = app_with(endpoint_urls);
        let base = Instant::now();
        for (i, offset) in [0.0_f64, 2.0].into_iter().enumerate() {
            // Bump every counter on the second pass so deltas exist.
            let text = if i == 0 {
                FIXTURE.to_string()
            } else {
                bump_counters(FIXTURE)
            };
            for endpoint in 0..app.endpoints.len() {
                app.handle_event(AppEvent::Scrape {
                    endpoint,
                    outcome: crate::state::ScrapeOutcome {
                        at: base + Duration::from_secs_f64(offset),
                        wall: std::time::SystemTime::now(),
                        duration: Duration::from_millis(10),
                        result: Ok(crate::state::ScrapePayload {
                            metrics: Some(crate::metrics::parse::parse_text(&text)),
                            ..Default::default()
                        }),
                    },
                });
            }
        }
        app
    }

    /// Scale every numeric sample up by 10%, so the second scrape of a
    /// fixture produces positive counter deltas.
    fn bump_counters(text: &str) -> String {
        text.lines()
            .map(|line| {
                if line.starts_with('#') || line.trim().is_empty() {
                    return line.to_string();
                }
                match line.rsplit_once(' ') {
                    Some((head, value)) => match value.parse::<f64>() {
                        Ok(v) => format!("{head} {}", v * 1.1),
                        Err(_) => line.to_string(),
                    },
                    None => line.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::testing::app_with as test_app;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn usage_loaded_ok_populates_cache_and_err_keeps_last_good_data() {
        let mut app = test_app(&["http://h:1"]);
        let data = crate::storage::usage::DailyUsage {
            days: Vec::new(),
            queried_at_ms: 5,
        };
        app.handle_event(AppEvent::UsageLoaded(Ok(data)));
        assert_eq!(app.usage.data.as_ref().map(|d| d.queried_at_ms), Some(5));
        assert!(app.usage.last_error.is_none());
        // A later failure keeps the last good data and surfaces the error.
        app.handle_event(AppEvent::UsageLoaded(Err("boom".into())));
        assert_eq!(app.usage.data.as_ref().map(|d| d.queried_at_ms), Some(5));
        assert_eq!(app.usage.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn usage_disabled_reason_is_kept_when_recording_unavailable() {
        // test_app runs with no env: no default data path can resolve.
        let app = test_app(&["http://h:1"]);
        assert!(app.recorder.is_none());
        assert!(app.usage.disabled.is_some());
    }

    #[test]
    fn counter_record_due_fires_immediately_then_throttles() {
        let mut app = test_app(&["http://h:1"]);
        let now = Instant::now();
        assert!(app.counter_record_due(0, now));
        app.counter_recorded_at[0] = Some(now);
        assert!(!app.counter_record_due(0, now + Duration::from_secs(29)));
        assert!(app.counter_record_due(0, now + COUNTER_RECORD_EVERY));
    }

    #[test]
    fn usage_query_never_due_without_a_recorder() {
        let app = test_app(&["http://h:1"]);
        assert!(!app.usage_query_due(Instant::now()));
    }

    #[test]
    fn t_cycles_panel_modes_only_in_endpoint_view() {
        let mut app = test_app(&["http://h:1", "http://h:2"]);
        assert_eq!(app.panel_mode, PanelMode::Overview);
        app.handle_key(key(KeyCode::Char('t'))); // fleet view: no-op
        assert_eq!(app.panel_mode, PanelMode::Overview);

        app.view = View::Endpoint(0);
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.panel_mode, PanelMode::Requests);
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.panel_mode, PanelMode::Tables);
        app.handle_key(key(KeyCode::Char('t')));
        assert_eq!(app.panel_mode, PanelMode::Overview);
        // Global, not per endpoint: tabbing must not reshape the screen.
        app.handle_key(key(KeyCode::Char('t')));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Endpoint(1));
        assert_eq!(app.panel_mode, PanelMode::Requests);
    }

    #[test]
    fn left_and_right_are_view_aliases() {
        let mut app = test_app(&["http://h:1", "http://h:2"]);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.view, View::Endpoint(0));
        assert_eq!(app.fleet_selected, 0);
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.view, View::Fleet);
    }

    #[test]
    fn request_log_event_reduces_into_endpoint_state() {
        let mut app = test_app(&["http://h:1"]);
        app.handle_event(AppEvent::RequestLog {
            endpoint: 0,
            at: Instant::now(),
            status: crate::logtail::TailStatus::Tailing,
            events: vec![crate::logtail::parse::LogEvent::Received {
                id: "cmpl-1".into(),
                max_tokens: Some(8),
            }],
        });
        assert_eq!(app.endpoints[0].requests.len(), 1);
        // Out-of-range endpoint indices are ignored, never panic.
        app.handle_event(AppEvent::RequestLog {
            endpoint: 99,
            at: Instant::now(),
            status: crate::logtail::TailStatus::Tailing,
            events: Vec::new(),
        });
    }

    #[test]
    fn tab_cycles_through_all_views() {
        let app = test_app(&["http://h1:1", "http://h2:2"]);
        let mut v = View::Fleet;
        let mut seen = vec![v];
        for _ in 0..3 {
            v = app.next_view(v);
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                View::Fleet,
                View::Endpoint(0),
                View::Endpoint(1),
                View::Fleet
            ]
        );
        // And backwards.
        assert_eq!(app.prev_view(View::Fleet), View::Endpoint(1));
        assert_eq!(app.prev_view(View::Endpoint(0)), View::Fleet);
        assert_eq!(app.prev_view(View::Endpoint(1)), View::Endpoint(0));
    }

    #[test]
    fn number_keys_select_endpoints_and_ignore_out_of_range() {
        let mut app = test_app(&["http://h1:1", "http://h2:2"]);
        app.handle_key(key(KeyCode::Char('2')));
        assert_eq!(app.view, View::Endpoint(0));
        app.handle_key(key(KeyCode::Char('3')));
        assert_eq!(app.view, View::Endpoint(1));
        // '9' is out of range with 2 endpoints: view unchanged.
        app.handle_key(key(KeyCode::Char('9')));
        assert_eq!(app.view, View::Endpoint(1));
        app.handle_key(key(KeyCode::Char('1')));
        assert_eq!(app.view, View::Fleet);
    }

    #[test]
    fn more_than_nine_endpoints_reachable_via_tab() {
        let urls: Vec<String> = (0..12).map(|i| format!("http://h{i}:80")).collect();
        let refs: Vec<&str> = urls.iter().map(String::as_str).collect();
        let app = test_app(&refs);
        // Endpoint 11 has no number key but Tab reaches it.
        let mut v = View::Fleet;
        for _ in 0..12 {
            v = app.next_view(v);
        }
        assert_eq!(v, View::Endpoint(11));
    }

    #[test]
    fn removed_view_keys_do_not_navigate() {
        let mut app = test_app(&["http://h1:1"]);
        // 'R' (old Raw view), 'H' (old History view), and lowercase 'r'
        // (refresh) must all leave the view unchanged.
        for k in ['R', 'H', 'h', 'r', '/', 'e', 'm'] {
            app.handle_key(key(KeyCode::Char(k)));
            assert_eq!(app.view, View::Fleet, "key {k:?} must not navigate");
        }
    }

    #[test]
    fn quit_keys() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.should_quit());

        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn interval_adjustment_clamps_at_one_second_and_minus_means_slower() {
        let mut app = test_app(&["http://h1:1"]);
        assert_eq!(app.refresh_interval, Duration::from_millis(1000));
        // '+' = faster = shorter interval (documented direction).
        app.handle_key(key(KeyCode::Char('+')));
        assert_eq!(app.refresh_interval, Duration::from_millis(MIN_REFRESH_MS));
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Char('+')));
        }
        assert_eq!(app.refresh_interval, Duration::from_millis(MIN_REFRESH_MS));
        for _ in 0..500 {
            app.handle_key(key(KeyCode::Char('-')));
        }
        assert_eq!(app.refresh_interval, Duration::from_millis(MAX_REFRESH_MS));
    }

    #[test]
    fn repeated_end_key_never_overflows_selection() {
        let mut app = test_app(&["http://h1:1", "http://h2:2"]);
        for _ in 0..5 {
            app.handle_key(key(KeyCode::Char('G')));
            app.handle_key(key(KeyCode::End));
        }
        // Clamped to the last row, no wrap/panic.
        assert_eq!(app.fleet_selected, 1);
    }

    #[test]
    fn chart_scroll_is_independent_of_row_selection_and_clamped() {
        let mut app = test_app(&["http://h1:1", "http://h2:2"]);
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.fleet_selected, 1);
        assert_eq!(app.fleet_chart_scroll, 0);
        for _ in 0..100 {
            app.handle_key(key(KeyCode::PageDown));
        }
        assert_eq!(app.fleet_chart_scroll, 16); // clamped
        assert_eq!(app.fleet_selected, 1); // selection untouched
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.fleet_chart_scroll, 0);
        assert_eq!(app.fleet_selected, 0);
    }

    #[test]
    fn pause_freezes_display_not_collection() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('p')));
        assert!(app.paused);
        app.dirty = false;
        // A scrape while paused still updates state but does not mark dirty.
        let outcome = crate::state::ScrapeOutcome {
            at: Instant::now(),
            wall: std::time::SystemTime::now(),
            duration: Duration::from_millis(1),
            result: Err("x".into()),
        };
        app.handle_event(AppEvent::Scrape {
            endpoint: 0,
            outcome,
        });
        assert!(!app.dirty);
        assert_eq!(app.endpoints[0].total_scrapes, 1);
    }

    #[test]
    fn metrics_started_event_updates_attempt_state() {
        let mut app = test_app(&["http://h1:1"]);
        let started = Instant::now();
        app.handle_event(AppEvent::MetricsStarted {
            endpoint: 0,
            at: started,
        });
        assert_eq!(app.endpoints[0].last_attempt_at, Some(started));
        assert_eq!(app.endpoints[0].scraping_since, Some(started));
    }

    #[test]
    fn optional_probe_event_updates_metadata_without_a_metrics_scrape() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_event(AppEvent::OptionalUpdate {
            endpoint: 0,
            healthy: Some(false),
            server_info: Some(crate::state::ServerInfo {
                version: Some("1.2.3".into()),
                max_running_requests: Some(48),
                ..Default::default()
            }),
            models: Some(vec![crate::state::ServedModel {
                id: "model-a".into(),
                root: Some("/models/model-a".into()),
                max_model_len: Some(262_144),
            }]),
        });
        let endpoint = &app.endpoints[0];
        assert_eq!(endpoint.healthy, Some(false));
        assert_eq!(endpoint.server_version.as_deref(), Some("1.2.3"));
        assert_eq!(endpoint.served_models.len(), 1);
        assert_eq!(endpoint.served_models[0].id, "model-a");
        assert_eq!(endpoint.served_models[0].max_model_len, Some(262_144));
        assert_eq!(endpoint.effective_max_running(None), Some(48));
        assert_eq!(endpoint.effective_max_running(Some(8)), Some(8));
        assert_eq!(endpoint.total_scrapes, 0);
    }

    #[test]
    fn help_overlay_swallows_navigation() {
        let mut app = test_app(&["http://h1:1"]);
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.view, View::Fleet); // unchanged
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.show_help);
    }
}

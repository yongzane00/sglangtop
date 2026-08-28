//! Events flowing into the main loop.

use crate::state::{ScrapeOutcome, ServerInfo};
use ratatui::crossterm::event::{KeyEvent, MouseEvent};
use std::time::Instant;

/// Everything the main select loop can receive, from collectors and the
/// input thread alike. One bounded channel carries them all so backpressure
/// applies uniformly.
#[derive(Debug)]
pub enum AppEvent {
    /// A non-overlapping `/metrics` request is about to start.
    MetricsStarted { endpoint: usize, at: Instant },
    /// Results from optional probes, published independently of `/metrics`.
    /// `None` means that field was not checked or unavailable this round.
    OptionalUpdate {
        endpoint: usize,
        healthy: Option<bool>,
        server_info: Option<ServerInfo>,
        models: Option<Vec<String>>,
    },
    /// A collector finished one poll cycle (success or failure).
    Scrape {
        endpoint: usize,
        outcome: ScrapeOutcome,
    },
    /// Result of the (blocking, off-loop) daily-usage query against the
    /// recorded history. Err strings are display-safe.
    UsageLoaded(Result<crate::storage::usage::DailyUsage, String>),
    /// A batch of parsed per-request log lines from one endpoint's tailer.
    RequestLog {
        endpoint: usize,
        /// Stamped by the tailer at read time; entries in a batch share it.
        at: Instant,
        status: crate::logtail::TailStatus,
        events: Vec<crate::logtail::parse::LogEvent>,
    },
    /// Keyboard input from the terminal.
    Key(KeyEvent),
    /// Mouse input (scrolling in lists).
    Mouse(MouseEvent),
    /// Terminal was resized; re-layout on next draw.
    Resize,
    /// The input thread died (stdin closed); exit gracefully.
    InputClosed,
}

//! A bounded, per-endpoint feed of things vllmtop itself observed.
//!
//! These are **our** observations, not the server's log: an endpoint went
//! unreachable, its counters reset (a restart), it preempted requests. Every
//! entry is derived from transitions already computed while reducing a
//! scrape, so the feed costs no extra I/O.
//!
//! Bounded in both directions: at most [`MAX_EVENTS`] entries, each detail
//! capped at [`MAX_DETAIL_BYTES`]. Repeats of the same event refresh the
//! timestamp instead of pushing, so a flapping endpoint cannot flood it.

use std::collections::VecDeque;
use std::time::Instant;

pub const MAX_EVENTS: usize = 64;
pub const MAX_DETAIL_BYTES: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// First successful scrape of this endpoint.
    Connected,
    /// Scraping started failing.
    Failed,
    /// Scraping succeeded again after failures.
    Recovered,
    /// Counters went backwards: the monitored server restarted.
    Restarted,
    /// The scheduler preempted running requests this interval.
    Preemption,
    /// Requests finished as error/abort this interval.
    Errors,
}

impl EventKind {
    /// Short label for the events panel.
    pub fn label(self) -> &'static str {
        match self {
            EventKind::Connected => "connected",
            EventKind::Failed => "failed",
            EventKind::Recovered => "recovered",
            EventKind::Restarted => "restarted",
            EventKind::Preemption => "preemption",
            EventKind::Errors => "errors",
        }
    }

    /// Whether this reads as a problem (drives styling).
    pub fn is_bad(self) -> bool {
        matches!(
            self,
            EventKind::Failed | EventKind::Restarted | EventKind::Errors
        )
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    /// Monotonic time we observed it (the panel renders an age).
    pub at: Instant,
    pub kind: EventKind,
    pub detail: String,
}

/// Newest at the back; the renderer iterates newest-first.
#[derive(Debug, Default)]
pub struct EventLog {
    entries: VecDeque<Event>,
}

impl EventLog {
    /// Record an observation. A consecutive repeat of the same
    /// `(kind, detail)` refreshes the existing entry's time rather than
    /// pushing a duplicate.
    pub fn push(&mut self, at: Instant, kind: EventKind, detail: impl Into<String>) {
        let detail = truncate_bytes(&detail.into(), MAX_DETAIL_BYTES);
        if let Some(last) = self.entries.back_mut()
            && last.kind == kind
            && last.detail == detail
        {
            last.at = at;
            return;
        }
        self.entries.push_back(Event { at, kind, detail });
        while self.entries.len() > MAX_EVENTS {
            self.entries.pop_front();
        }
    }

    pub fn iter_newest_first(&self) -> impl Iterator<Item = &Event> {
        self.entries.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn ring_is_bounded_and_evicts_oldest() {
        let mut log = EventLog::default();
        let t0 = Instant::now();
        for i in 0..MAX_EVENTS + 10 {
            log.push(t0, EventKind::Errors, format!("burst {i}"));
        }
        assert_eq!(log.len(), MAX_EVENTS);
        let newest = log.iter_newest_first().next().unwrap();
        assert_eq!(newest.detail, format!("burst {}", MAX_EVENTS + 9));
        assert!(!log.iter_newest_first().any(|e| e.detail == "burst 0"));
    }

    #[test]
    fn consecutive_repeats_refresh_instead_of_flooding() {
        let mut log = EventLog::default();
        let t0 = Instant::now();
        log.push(t0, EventKind::Failed, "connection refused");
        log.push(
            t0 + Duration::from_secs(1),
            EventKind::Failed,
            "connection refused",
        );
        assert_eq!(log.len(), 1);
        assert!(log.iter_newest_first().next().unwrap().at > t0);
        // A different detail is a genuinely new event.
        log.push(t0 + Duration::from_secs(2), EventKind::Failed, "timed out");
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn detail_is_capped_on_a_char_boundary() {
        let mut log = EventLog::default();
        log.push(Instant::now(), EventKind::Errors, "ẞ".repeat(200));
        let detail = &log.iter_newest_first().next().unwrap().detail;
        assert!(detail.len() <= MAX_DETAIL_BYTES);
        assert!(detail.chars().all(|c| c == 'ẞ'));
    }

    #[test]
    fn newest_first_ordering() {
        let mut log = EventLog::default();
        let t0 = Instant::now();
        log.push(t0, EventKind::Connected, "first scrape ok");
        log.push(t0, EventKind::Restarted, "counters reset");
        let kinds: Vec<_> = log.iter_newest_first().map(|e| e.kind).collect();
        assert_eq!(kinds, vec![EventKind::Restarted, EventKind::Connected]);
    }
}

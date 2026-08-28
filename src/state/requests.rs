//! Bounded in-memory ring of recent requests, fed by the log tailer.
//!
//! Privacy firewall: prompt previews live ONLY here. The recorder reads
//! exclusively `EndpointState::{current,cumulative}_samples()`, which never
//! touch this ring, so previews cannot reach SQLite, logs, or disk.

use std::collections::VecDeque;
use std::time::Instant;

use crate::logtail::TailStatus;
use crate::logtail::parse::LogEvent;

/// Ring capacity: enough to fill any terminal, bounded memory.
pub const MAX_ENTRIES: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestStatus {
    Generating,
    Finished { reason: Option<String> },
}

#[derive(Debug, Clone)]
pub struct RequestEntry {
    pub id: String,
    /// When the tailer read the "Received" line (age column).
    pub seen_at: Instant,
    pub max_tokens: Option<u64>,
    /// `None` renders `--`: prompt previews exist only when the server logs
    /// at DEBUG level.
    pub prompt_preview: Option<String>,
    pub status: RequestStatus,
}

/// Newest entries at the back; the renderer iterates newest-first.
#[derive(Debug, Default)]
pub struct RequestLog {
    entries: VecDeque<RequestEntry>,
    /// `None` until a tailer reports (i.e. no `log_file` configured).
    pub tail_status: Option<TailStatus>,
}

impl RequestLog {
    pub fn apply(&mut self, at: Instant, status: TailStatus, events: Vec<LogEvent>) {
        self.tail_status = Some(status);
        for event in events {
            match event {
                LogEvent::Received { id, max_tokens } => {
                    if let Some(existing) = self.find_mut(&id) {
                        // Duplicate "Received" (e.g. log replay): refresh.
                        existing.seen_at = at;
                        existing.max_tokens = max_tokens;
                        existing.status = RequestStatus::Generating;
                        continue;
                    }
                    self.entries.push_back(RequestEntry {
                        id,
                        seen_at: at,
                        max_tokens,
                        prompt_preview: None,
                        status: RequestStatus::Generating,
                    });
                    while self.entries.len() > MAX_ENTRIES {
                        self.entries.pop_front();
                    }
                }
                LogEvent::Details { id, prompt_preview } => {
                    // Unknown id (arrived before start, or evicted): ignored —
                    // a stub row with no honest age would be dishonest.
                    if let Some(existing) = self.find_mut(&id) {
                        existing.prompt_preview = Some(prompt_preview);
                    }
                }
                LogEvent::Finished { id, finish_reason } => {
                    if let Some(existing) = self.find_mut(&id) {
                        existing.status = RequestStatus::Finished {
                            reason: finish_reason,
                        };
                    }
                }
            }
        }
    }

    /// Recent ids live near the back: scan from there (ring is ≤200 long).
    fn find_mut(&mut self, id: &str) -> Option<&mut RequestEntry> {
        self.entries.iter_mut().rev().find(|e| e.id == id)
    }

    pub fn iter_newest_first(&self) -> impl Iterator<Item = &RequestEntry> {
        self.entries.iter().rev()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn received(id: &str, max_tokens: Option<u64>) -> LogEvent {
        LogEvent::Received {
            id: id.into(),
            max_tokens,
        }
    }

    #[test]
    fn ring_bounded_at_cap_evicts_oldest() {
        let mut log = RequestLog::default();
        let now = Instant::now();
        let events: Vec<LogEvent> = (0..MAX_ENTRIES + 10)
            .map(|i| received(&format!("cmpl-{i}"), Some(1)))
            .collect();
        log.apply(now, TailStatus::Tailing, events);
        assert_eq!(log.len(), MAX_ENTRIES);
        // Newest first; the oldest ten were evicted.
        assert_eq!(
            log.iter_newest_first().next().map(|e| e.id.as_str()),
            Some(format!("cmpl-{}", MAX_ENTRIES + 9).as_str())
        );
        assert!(!log.iter_newest_first().any(|e| e.id == "cmpl-5"));
    }

    #[test]
    fn details_and_finished_merge_by_id() {
        let mut log = RequestLog::default();
        let now = Instant::now();
        log.apply(
            now,
            TailStatus::Tailing,
            vec![
                received("cmpl-1", Some(64)),
                LogEvent::Details {
                    id: "cmpl-1".into(),
                    prompt_preview: "An example question".into(),
                },
                LogEvent::Finished {
                    id: "cmpl-1".into(),
                    finish_reason: Some("stop".into()),
                },
            ],
        );
        assert_eq!(log.len(), 1);
        let entry = log.iter_newest_first().next().unwrap();
        assert_eq!(entry.prompt_preview.as_deref(), Some("An example question"));
        assert_eq!(
            entry.status,
            RequestStatus::Finished {
                reason: Some("stop".into())
            }
        );
        assert_eq!(entry.max_tokens, Some(64));
    }

    #[test]
    fn unknown_id_details_ignored() {
        let mut log = RequestLog::default();
        log.apply(
            Instant::now(),
            TailStatus::Tailing,
            vec![LogEvent::Details {
                id: "never-seen".into(),
                prompt_preview: "x".into(),
            }],
        );
        assert!(log.is_empty());
    }

    #[test]
    fn duplicate_received_refreshes_entry_without_duplicating() {
        let mut log = RequestLog::default();
        let t0 = Instant::now();
        log.apply(t0, TailStatus::Tailing, vec![received("cmpl-1", Some(8))]);
        log.apply(
            t0 + std::time::Duration::from_secs(1),
            TailStatus::Tailing,
            vec![received("cmpl-1", Some(16))],
        );
        assert_eq!(log.len(), 1);
        let entry = log.iter_newest_first().next().unwrap();
        assert_eq!(entry.max_tokens, Some(16));
        assert!(entry.seen_at > t0);
    }

    #[test]
    fn tail_status_tracks_latest_report() {
        let mut log = RequestLog::default();
        assert_eq!(log.tail_status, None);
        log.apply(Instant::now(), TailStatus::FileMissing, Vec::new());
        assert_eq!(log.tail_status, Some(TailStatus::FileMissing));
        log.apply(Instant::now(), TailStatus::Tailing, Vec::new());
        assert_eq!(log.tail_status, Some(TailStatus::Tailing));
    }
}

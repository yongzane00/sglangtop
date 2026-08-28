//! Poll-based log tailers: one tokio task per endpoint that has `log_file`
//! configured. Follows the file like `tail -f` (first open seeks to EOF, so
//! only requests arriving after startup appear and ages stay honest);
//! reopens the path each tick, which transparently follows rotation-by-
//! rename; a shrunken file (truncation/copytruncate) restarts from offset 0.
//!
//! Bounded in every dimension: bytes read per tick, line length, events per
//! batch. Prompt previews parsed here are held only in the in-memory request
//! ring — never recorded, logged, or persisted.

pub mod parse;

use std::io::SeekFrom;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::event::AppEvent;

pub const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// At most this many bytes are read per poll; a backlog drains over ticks.
pub const MAX_READ_PER_TICK: usize = 256 * 1024;
/// Lines longer than this are dropped (bounded partial-line carry).
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// At most this many parsed events per batch (newest kept).
pub const MAX_EVENTS_PER_TICK: usize = 256;

/// Tailer health, shown by the requests pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailStatus {
    FileMissing,
    Tailing,
}

/// Spawn one tailer per endpoint with a configured `log_file`.
pub fn spawn_all(config: &Config, events: mpsc::Sender<AppEvent>) {
    for (index, endpoint) in config.endpoints.iter().enumerate() {
        let Some(path) = &endpoint.log_file else {
            continue;
        };
        let task = TailerTask::new(index, path.clone(), events.clone());
        tokio::spawn(task.run());
    }
}

struct TailerTask {
    endpoint: usize,
    path: PathBuf,
    events: mpsc::Sender<AppEvent>,
    offset: u64,
    /// First successful open seeks to EOF; a file that appears later is new,
    /// so everything in it is new and is read from the start.
    initialized: bool,
    /// Partial trailing line carried between polls (capped).
    carry: Vec<u8>,
    /// Currently discarding an oversized line until its newline.
    skipping_oversized: bool,
    last_status: Option<TailStatus>,
}

impl TailerTask {
    fn new(endpoint: usize, path: PathBuf, events: mpsc::Sender<AppEvent>) -> TailerTask {
        TailerTask {
            endpoint,
            path,
            events,
            offset: 0,
            initialized: false,
            carry: Vec::new(),
            skipping_oversized: false,
            last_status: None,
        }
    }

    async fn run(mut self) {
        let mut tick = tokio::time::interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let (status, events) = self.poll_step().await;
            let status_changed = self.last_status != Some(status);
            if events.is_empty() && !status_changed {
                continue;
            }
            self.last_status = Some(status);
            let sent = self.events.try_send(AppEvent::RequestLog {
                endpoint: self.endpoint,
                at: Instant::now(),
                status,
                events,
            });
            match sent {
                Ok(()) => {}
                // A full channel just drops this tick's batch (bounded,
                // self-healing); a closed one means shutdown.
                Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            }
        }
    }

    /// One poll: stat/open/seek/read, split lines, parse. Directly unit-
    /// testable without timing.
    async fn poll_step(&mut self) -> (TailStatus, Vec<parse::LogEvent>) {
        let Ok(meta) = tokio::fs::metadata(&self.path).await else {
            // Missing file: whatever appears later is entirely new content.
            self.initialized = true;
            self.offset = 0;
            self.carry.clear();
            self.skipping_oversized = false;
            return (TailStatus::FileMissing, Vec::new());
        };
        if !self.initialized {
            // First sight of an existing file: start at its end.
            self.initialized = true;
            self.offset = meta.len();
            return (TailStatus::Tailing, Vec::new());
        }
        if meta.len() < self.offset {
            // Truncated or rotated-in-place: restart from the top.
            self.offset = 0;
            self.carry.clear();
            self.skipping_oversized = false;
        }
        if meta.len() == self.offset {
            return (TailStatus::Tailing, Vec::new());
        }

        let Ok(mut file) = tokio::fs::File::open(&self.path).await else {
            return (TailStatus::FileMissing, Vec::new());
        };
        if file.seek(SeekFrom::Start(self.offset)).await.is_err() {
            return (TailStatus::Tailing, Vec::new());
        }
        let want = usize::try_from(meta.len() - self.offset)
            .unwrap_or(MAX_READ_PER_TICK)
            .min(MAX_READ_PER_TICK);
        let mut buf = vec![0u8; want];
        let mut filled = 0usize;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]).await {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => break,
            }
        }
        buf.truncate(filled);
        self.offset += filled as u64;

        let mut events = Vec::new();
        self.consume_bytes(&buf, &mut events);
        if events.len() > MAX_EVENTS_PER_TICK {
            let excess = events.len() - MAX_EVENTS_PER_TICK;
            events.drain(..excess); // keep the newest
        }
        (TailStatus::Tailing, events)
    }

    /// Line assembly over the carry buffer (sync, allocation-bounded).
    fn consume_bytes(&mut self, chunk: &[u8], out: &mut Vec<parse::LogEvent>) {
        for &b in chunk {
            if b == b'\n' {
                if self.skipping_oversized {
                    self.skipping_oversized = false;
                } else if !self.carry.is_empty() {
                    let line = String::from_utf8_lossy(&self.carry);
                    if let Some(event) = parse::parse_line(&line) {
                        out.push(event);
                    }
                }
                self.carry.clear();
                continue;
            }
            if self.skipping_oversized {
                continue;
            }
            if self.carry.len() >= MAX_LINE_BYTES {
                // Too long to be a request-logger line worth keeping.
                self.carry.clear();
                self.skipping_oversized = true;
                continue;
            }
            self.carry.push(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn task_for(path: PathBuf) -> TailerTask {
        let (tx, _rx) = mpsc::channel(8);
        TailerTask::new(0, path, tx)
    }

    fn append(path: &std::path::Path, text: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    const RECEIVED: &str = "Received request cmpl-1: params: SamplingParams(max_tokens=8)\n";

    #[tokio::test]
    async fn starts_at_end_of_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sglang.log");
        append(&path, RECEIVED); // pre-existing: must be invisible
        let mut task = task_for(path.clone());
        let (status, events) = task.poll_step().await;
        assert_eq!(status, TailStatus::Tailing);
        assert!(events.is_empty());
        append(
            &path,
            "Received request cmpl-2: params: SamplingParams(max_tokens=9)\n",
        );
        let (_, events) = task.poll_step().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], parse::LogEvent::Received { id, .. } if id == "cmpl-2"));
    }

    #[tokio::test]
    async fn missing_file_reports_status_then_reads_from_start_when_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-yet.log");
        let mut task = task_for(path.clone());
        let (status, _) = task.poll_step().await;
        assert_eq!(status, TailStatus::FileMissing);
        // The file appearing later is new: read from offset 0.
        append(&path, RECEIVED);
        let (status, events) = task.poll_step().await;
        assert_eq!(status, TailStatus::Tailing);
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn truncation_resets_offset_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sglang.log");
        // Longer than the post-truncation content: shrink detection is
        // size-based (a rotated-in file LONGER than the old offset is the
        // documented undetectable case).
        append(&path, &"old content\n".repeat(20));
        let mut task = task_for(path.clone());
        let _ = task.poll_step().await; // seeks to EOF
        std::fs::write(&path, RECEIVED).unwrap(); // shrink: truncate+rewrite
        let (_, events) = task.poll_step().await;
        assert_eq!(events.len(), 1);
    }

    #[tokio::test]
    async fn partial_line_held_across_polls_until_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sglang.log");
        std::fs::write(&path, "").unwrap();
        let mut task = task_for(path.clone());
        let _ = task.poll_step().await;
        append(&path, "Received request cmpl-1: params: Sampling");
        let (_, events) = task.poll_step().await;
        assert!(events.is_empty());
        append(&path, "Params(max_tokens=8)\n");
        let (_, events) = task.poll_step().await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            parse::LogEvent::Received {
                max_tokens: Some(8),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn oversized_line_is_bounded_and_following_line_still_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sglang.log");
        std::fs::write(&path, "").unwrap();
        let mut task = task_for(path.clone());
        let _ = task.poll_step().await;
        let huge = "x".repeat(MAX_LINE_BYTES + 100);
        append(&path, &format!("{huge}\n{RECEIVED}"));
        let (_, events) = task.poll_step().await;
        assert_eq!(events.len(), 1);
        assert!(task.carry.len() <= MAX_LINE_BYTES);
    }

    #[tokio::test]
    async fn non_utf8_bytes_are_lossy_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sglang.log");
        std::fs::write(&path, "").unwrap();
        let mut task = task_for(path.clone());
        let _ = task.poll_step().await;
        let mut bytes = b"Received request cmpl-\xff1: params: SamplingParams()\n".to_vec();
        bytes.extend_from_slice(RECEIVED.as_bytes());
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&bytes).unwrap();
        }
        let (_, events) = task.poll_step().await;
        // Both lines parse; the lossy one keeps its replacement character.
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn read_cap_per_tick_bounds_each_poll_and_catches_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sglang.log");
        std::fs::write(&path, "").unwrap();
        let mut task = task_for(path.clone());
        let _ = task.poll_step().await;
        // Well past one tick's read budget of noise, then a real line.
        let filler_line = format!("{}\n", "y".repeat(1000));
        let filler = filler_line.repeat((MAX_READ_PER_TICK / 1000) + 50);
        append(&path, &format!("{filler}{RECEIVED}"));
        let mut all = Vec::new();
        for _ in 0..10 {
            let (_, mut events) = task.poll_step().await;
            all.append(&mut events);
        }
        assert_eq!(all.len(), 1, "catches up over successive polls");
    }

    #[tokio::test]
    async fn events_per_tick_capped_keeping_newest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sglang.log");
        std::fs::write(&path, "").unwrap();
        let mut task = task_for(path.clone());
        let _ = task.poll_step().await;
        let mut text = String::new();
        for i in 0..(MAX_EVENTS_PER_TICK + 20) {
            text.push_str(&format!(
                "Received request cmpl-{i}: params: SamplingParams(max_tokens=1)\n"
            ));
        }
        append(&path, &text);
        let (_, events) = task.poll_step().await;
        assert_eq!(events.len(), MAX_EVENTS_PER_TICK);
        let last_id = format!("cmpl-{}", MAX_EVENTS_PER_TICK + 19);
        assert!(
            matches!(events.last(), Some(parse::LogEvent::Received { id, .. }) if *id == last_id)
        );
    }
}

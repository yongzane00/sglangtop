//! SQLite recording of aggregate metric samples (on by default; opt out
//! with `--no-record`).
//!
//! Design constraints (docs/PLAN.md):
//! - Runs on a dedicated OS thread; SQLite never touches tokio workers or
//!   the render loop.
//! - The channel from the reducer is bounded and non-blocking: when the
//!   recorder falls behind, batches are DROPPED and counted, collection is
//!   never delayed.
//! - Stores aggregate endpoint/model samples only — never prompts, request
//!   bodies, tokens, or headers.
//! - Retention cleanup runs periodically in the same thread, batched so it
//!   cannot hold long locks.
//! - Reads (the daily-usage query in [`usage`]) happen on a separate
//!   read-only connection; WAL keeps them from ever blocking the writer.

pub mod usage;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

pub const SCHEMA_VERSION: i64 = 1;
/// Bounded queue between reducer and recorder thread (batches, not rows).
const QUEUE_CAPACITY: usize = 64;
/// Run retention cleanup at most this often.
const CLEANUP_EVERY: Duration = Duration::from_secs(600);
/// Delete at most this many rows per cleanup transaction.
const CLEANUP_CHUNK: usize = 10_000;
/// At most this many chunks per cleanup pass: bounds how long one pass can
/// occupy the writer thread (Recorder::shutdown joins it while the terminal
/// is still in raw mode, so a pass must never run long). A backlog simply
/// drains over successive passes.
const CLEANUP_MAX_CHUNKS: usize = 4;

/// One recorded sample row.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleRow {
    /// Wall-clock milliseconds since the Unix epoch.
    pub ts_ms: i64,
    pub endpoint: String,
    pub model: String,
    pub engine: Option<String>,
    /// Metric id from [`crate::state::series_id`].
    pub metric: String,
    pub value: f64,
}

enum Msg {
    Batch(Vec<SampleRow>),
    Shutdown,
}

/// Handle owned by the reducer side.
pub struct Recorder {
    tx: SyncSender<Msg>,
    thread: Option<std::thread::JoinHandle<()>>,
    dropped: Arc<AtomicU64>,
    /// Rows confirmed written by the recorder thread.
    written: Arc<AtomicU64>,
    path: PathBuf,
}

impl Recorder {
    /// Open (or create) the database and start the writer thread. Errors here
    /// are fatal for recording only; the caller shows them and continues.
    pub fn start(path: &Path, retention_days: u32) -> Result<Recorder, String> {
        // The default data-dir path may not exist yet (first run).
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let conn = open_database(path)?;
        let (tx, rx) = std::sync::mpsc::sync_channel::<Msg>(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let written = Arc::new(AtomicU64::new(0));
        let written_in_thread = Arc::clone(&written);
        let thread = std::thread::Builder::new()
            .name("sglangtop-recorder".into())
            .spawn(move || writer_loop(conn, rx, retention_days, written_in_thread))
            .map_err(|e| format!("failed to spawn recorder thread: {e}"))?;
        Ok(Recorder {
            tx,
            thread: Some(thread),
            dropped,
            written,
            path: path.to_path_buf(),
        })
    }

    /// Queue a batch without blocking. Full queue ⇒ batch dropped + counted.
    pub fn record(&self, batch: Vec<SampleRow>) {
        if batch.is_empty() {
            return;
        }
        match self.tx.try_send(Msg::Batch(batch)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Batches dropped because the recorder could not keep up.
    pub fn dropped_batches(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Rows verified written (used by the UI so "recording" is never claimed
    /// without actual writes).
    pub fn rows_written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Flush and stop. Called on normal exit.
    pub fn shutdown(mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn open_database(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    // WAL keeps writers from blocking any future readers and batches fsyncs.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| format!("set WAL mode: {e}"))?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| format!("set synchronous: {e}"))?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<(), String> {
    init_schema_sql(conn).map_err(|e| format!("initialize schema: {e}"))?;
    check_schema_version(conn)
}

fn init_schema_sql(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
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
         -- Serves the daily-usage query (metric IN (...) AND ts_ms >= ...).
         -- Additive indexes never bump SCHEMA_VERSION.
         CREATE INDEX IF NOT EXISTS idx_samples_metric_ts
             ON samples (metric, ts_ms);",
    )
}

fn check_schema_version(conn: &Connection) -> Result<(), String> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(|e| format!("read schema version: {e}"))?;
    match existing {
        None => {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                [SCHEMA_VERSION.to_string()],
            )
            .map_err(|e| format!("stamp schema version: {e}"))?;
            Ok(())
        }
        Some(v) if v == SCHEMA_VERSION.to_string() => Ok(()),
        // Future migrations hook in here; v1 only refuses to touch
        // databases from a newer sglangtop.
        Some(v) => Err(format!(
            "database schema version {v} is not supported by this build \
             (expected {SCHEMA_VERSION})"
        )),
    }
}

fn writer_loop(
    mut conn: Connection,
    rx: Receiver<Msg>,
    retention_days: u32,
    written: Arc<AtomicU64>,
) {
    let mut last_cleanup = std::time::Instant::now()
        .checked_sub(CLEANUP_EVERY)
        .unwrap_or_else(std::time::Instant::now);
    // Exits on Msg::Shutdown or when every sender is gone.
    while let Ok(Msg::Batch(rows)) = rx.recv() {
        match write_batch(&mut conn, &rows) {
            Ok(n) => {
                written.fetch_add(n, Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!("recorder: batch write failed: {e}");
            }
        }
        if last_cleanup.elapsed() >= CLEANUP_EVERY {
            last_cleanup = std::time::Instant::now();
            if let Err(e) = cleanup(&conn, retention_days) {
                tracing::warn!("recorder: retention cleanup failed: {e}");
            }
        }
    }
    // Best-effort final drain of anything still queued.
    while let Ok(Msg::Batch(rows)) = rx.try_recv() {
        if let Ok(n) = write_batch(&mut conn, &rows) {
            written.fetch_add(n, Ordering::Relaxed);
        }
    }
    let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
}

fn write_batch(conn: &mut Connection, rows: &[SampleRow]) -> rusqlite::Result<u64> {
    let tx = conn.transaction()?;
    let mut n = 0u64;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO samples (ts_ms, endpoint, model, engine, metric, value)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for row in rows {
            stmt.execute(rusqlite::params![
                row.ts_ms,
                row.endpoint,
                row.model,
                row.engine,
                row.metric,
                row.value,
            ])?;
            n += 1;
        }
    }
    tx.commit()?;
    Ok(n)
}

/// Delete rows older than the retention window, in bounded chunks, with a
/// bounded number of chunks per call.
fn cleanup(conn: &Connection, retention_days: u32) -> rusqlite::Result<usize> {
    let cutoff = now_ms() - i64::from(retention_days) * 86_400_000;
    let mut total = 0usize;
    for _ in 0..CLEANUP_MAX_CHUNKS {
        let deleted = conn.execute(
            "DELETE FROM samples WHERE rowid IN (
                 SELECT rowid FROM samples WHERE ts_ms < ?1 LIMIT ?2
             )",
            rusqlite::params![cutoff, CLEANUP_CHUNK as i64],
        )?;
        total += deleted;
        if deleted < CLEANUP_CHUNK {
            break;
        }
    }
    Ok(total)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts_ms: i64, metric: &str, value: f64) -> SampleRow {
        SampleRow {
            ts_ms,
            endpoint: "ep".into(),
            model: "m".into(),
            engine: Some("0".into()),
            metric: metric.into(),
            value,
        }
    }

    fn count_rows(path: &Path) -> i64 {
        let conn = Connection::open(path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM samples", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn writes_are_verified_and_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let recorder = Recorder::start(&path, 30).unwrap();
        // Timestamps must be recent: rows older than the retention window
        // are (correctly) removed by the cleanup pass.
        let t0 = now_ms();
        recorder.record(vec![row(t0, "running", 2.0), row(t0, "waiting", 0.0)]);
        recorder.record(vec![row(t0 + 1_000, "running", 3.0)]);

        // Wait for confirmed writes rather than sleeping blindly.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while recorder.rows_written() < 3 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(recorder.rows_written(), 3);
        recorder.shutdown();

        assert_eq!(count_rows(&path), 3);
        let conn = Connection::open(&path).unwrap();
        let v: f64 = conn
            .query_row(
                "SELECT value FROM samples WHERE metric = 'running' AND ts_ms = ?1",
                [t0 + 1_000],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 3.0);
    }

    #[test]
    fn ancient_rows_are_cleaned_up_even_when_freshly_written() {
        // Documents the interaction that bit the first version of the tests
        // above: cleanup runs after the first batch and removes anything
        // outside the retention window, including rows just written.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let recorder = Recorder::start(&path, 30).unwrap();
        recorder.record(vec![row(1_000, "running", 1.0)]); // 1970!
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while recorder.rows_written() < 1 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        recorder.shutdown();
        assert_eq!(count_rows(&path), 0);
    }

    #[test]
    fn schema_version_is_stamped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        Recorder::start(&path, 30).unwrap().shutdown();
        let conn = Connection::open(&path).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        {
            let conn = Connection::open(&path).unwrap();
            init_schema(&conn).unwrap();
            conn.execute(
                "UPDATE meta SET value = '999' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        // (`.err().unwrap()` because Recorder is deliberately non-Debug.)
        let err = Recorder::start(&path, 30).err().unwrap();
        assert!(err.contains("999"), "{err}");
    }

    #[test]
    fn retention_cleanup_removes_only_old_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let conn = open_database(&path).unwrap();
        let now = now_ms();
        let old = now - 40i64 * 86_400_000; // 40 days ago
        let mut c = Connection::open(&path).unwrap();
        write_batch(
            &mut c,
            &[
                row(old, "running", 1.0),
                row(now - 1_000, "running", 2.0),
                row(now, "running", 3.0),
            ],
        )
        .unwrap();
        let deleted = cleanup(&conn, 30).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(count_rows(&path), 2);
        // Idempotent.
        assert_eq!(cleanup(&conn, 30).unwrap(), 0);
    }

    #[test]
    fn reopening_existing_database_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let t0 = now_ms();
        let r1 = Recorder::start(&path, 30).unwrap();
        r1.record(vec![row(t0, "a", 1.0)]);
        r1.shutdown();
        let r2 = Recorder::start(&path, 30).unwrap();
        r2.record(vec![row(t0 + 1, "b", 2.0)]);
        r2.shutdown();
        assert_eq!(count_rows(&path), 2);
    }
}

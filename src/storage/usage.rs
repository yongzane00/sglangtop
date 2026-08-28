//! Read-side of the recorder: per-day usage totals for the fleet charts.
//!
//! Runs on a second, read-only connection (WAL keeps readers and the writer
//! thread from blocking each other). `query_daily_usage` is blocking — call
//! it from `tokio::task::spawn_blocking`, never on the UI/reducer path.
//!
//! Day totals are reconstructed from cumulative counter snapshots as the sum
//! of *positive* consecutive deltas per (endpoint, model, engine, metric)
//! partition, bucketed by the LOCAL calendar day of the later sample:
//! - within a monotonic run the deltas telescope to segment max−min;
//! - a server restart makes one delta negative → clamped to 0 (bounded loss:
//!   at most one recording cadence of traffic around the restart);
//! - the midnight-straddling delta is attributed wholly to the later day
//!   (error ≤ one cadence per boundary — never silently dropped);
//! - a partition with a single sample yields no delta → the day is ABSENT
//!   (`--`), while a flat counter yields a real 0. Absent ≠ zero.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::state::series_id;

/// The fleet charts cover this many trailing local days.
pub const USAGE_WINDOW_DAYS: usize = 30;

/// One local day's fleet-wide totals. Absent metrics were not observed that
/// day (endpoint down, recording off, counter not exported) — never zero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DayUsage {
    /// Local "YYYY-MM-DD".
    pub day: String,
    pub prompt_tokens: Option<f64>,
    pub generation_tokens: Option<f64>,
    pub requests: Option<f64>,
}

/// Result of one usage query: exactly [`USAGE_WINDOW_DAYS`] entries, one per
/// local calendar day ascending (today last). Days without observations keep
/// `None` fields — the renderer shows them as `--`, never as zero.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DailyUsage {
    pub days: Vec<DayUsage>,
    /// Wall-clock ms of the query, for "as of" display.
    pub queried_at_ms: i64,
}

/// Aggregate the recorded cumulative counters into per-local-day totals.
/// Blocking; error strings are display-safe (no secrets flow through here —
/// the samples table stores endpoint names, never URLs or headers).
pub fn query_daily_usage(path: &Path) -> Result<DailyUsage, String> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open {} read-only: {e}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .map_err(|e| format!("set busy timeout: {e}"))?;
    query_on(&conn)
}

fn query_on(conn: &Connection) -> Result<DailyUsage, String> {
    let queried_at_ms = super::now_ms();
    // 31 days of raw samples so day 30's first delta has its predecessor.
    let min_ts = queried_at_ms - 31 * 86_400_000;

    let metric_list = series_id::CUMULATIVE
        .iter()
        .map(|m| format!("'{m}'"))
        .collect::<Vec<_>>()
        .join(",");
    // `metric` values are compile-time constants (series_id::CUMULATIVE), so
    // interpolating the IN list is safe; the only runtime input is bound.
    let sql = format!(
        "WITH bounded AS (
             SELECT ts_ms, endpoint, model, engine, metric, value
             FROM samples
             WHERE metric IN ({metric_list}) AND ts_ms >= ?1
         ),
         deltas AS (
             SELECT strftime('%Y-%m-%d', ts_ms / 1000, 'unixepoch', 'localtime') AS day,
                    metric,
                    value - LAG(value) OVER (
                        PARTITION BY endpoint, model, engine, metric
                        ORDER BY ts_ms
                    ) AS d
             FROM bounded
         )
         SELECT day, metric, SUM(MAX(d, 0.0)) AS total
         FROM deltas
         WHERE d IS NOT NULL
           AND day >= strftime('%Y-%m-%d', 'now', 'localtime', '-{days} days')
         GROUP BY day, metric
         ORDER BY day ASC",
        days = USAGE_WINDOW_DAYS - 1,
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("prepare usage query: {e}"))?;
    let rows = stmt
        .query_map([min_ts], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
            ))
        })
        .map_err(|e| format!("run usage query: {e}"))?;

    // BTreeMap keeps ISO day strings sorted for free.
    let mut by_day: BTreeMap<String, DayUsage> = BTreeMap::new();
    for row in rows {
        let (day, metric, total) = row.map_err(|e| format!("read usage row: {e}"))?;
        let entry = by_day.entry(day.clone()).or_insert_with(|| DayUsage {
            day,
            ..DayUsage::default()
        });
        match metric.as_str() {
            series_id::PROMPT_TOKENS_TOTAL => entry.prompt_tokens = Some(total),
            series_id::GENERATION_TOKENS_TOTAL => entry.generation_tokens = Some(total),
            series_id::REQUESTS_TOTAL => entry.requests = Some(total),
            _ => {}
        }
    }

    // Full calendar axis from SQLite (std Rust has no local-date math):
    // 30 local day strings ascending, today last.
    let mut axis_stmt = conn
        .prepare(
            "WITH RECURSIVE nums(n) AS (
                 SELECT 0 UNION ALL SELECT n + 1 FROM nums WHERE n < ?1 - 1
             )
             SELECT strftime('%Y-%m-%d', 'now', 'localtime',
                             '-' || (?1 - 1 - n) || ' days')
             FROM nums ORDER BY n ASC",
        )
        .map_err(|e| format!("prepare axis query: {e}"))?;
    let axis = axis_stmt
        .query_map([USAGE_WINDOW_DAYS as i64], |r| r.get::<_, String>(0))
        .map_err(|e| format!("run axis query: {e}"))?
        .collect::<Result<Vec<String>, _>>()
        .map_err(|e| format!("read axis row: {e}"))?;

    let days = axis
        .into_iter()
        .map(|day| {
            by_day.remove(&day).unwrap_or(DayUsage {
                day,
                ..DayUsage::default()
            })
        })
        .collect();
    Ok(DailyUsage {
        days,
        queried_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::now_ms;

    /// An in-memory DB with the production schema.
    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        super::super::init_schema_sql(&conn).unwrap();
        conn
    }

    fn insert(conn: &Connection, ts_ms: i64, endpoint: &str, metric: &str, value: f64) {
        conn.execute(
            "INSERT INTO samples (ts_ms, endpoint, model, engine, metric, value)
             VALUES (?1, ?2, 'm', '0', ?3, ?4)",
            rusqlite::params![ts_ms, endpoint, metric, value],
        )
        .unwrap();
    }

    /// Ask SQLite for the local day of a timestamp so tests are
    /// timezone-independent.
    fn local_day(conn: &Connection, ts_ms: i64) -> String {
        conn.query_row(
            "SELECT strftime('%Y-%m-%d', ?1 / 1000, 'unixepoch', 'localtime')",
            [ts_ms],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// The axis entry for the local day containing `ts_ms`.
    fn on<'a>(conn: &Connection, usage: &'a DailyUsage, ts_ms: i64) -> &'a DayUsage {
        let day = local_day(conn, ts_ms);
        usage.days.iter().find(|d| d.day == day).unwrap()
    }

    #[test]
    fn sums_positive_deltas_per_local_day() {
        let conn = test_db();
        let t0 = now_ms() - 3_600_000; // one hour ago: safely inside today
        for (i, v) in [100.0, 250.0, 400.0].iter().enumerate() {
            insert(
                &conn,
                t0 + i as i64 * 30_000,
                "a",
                "generation_tokens_total",
                *v,
            );
        }
        let usage = query_on(&conn).unwrap();
        // Always the full 30-day calendar axis, ascending, today last.
        assert_eq!(usage.days.len(), USAGE_WINDOW_DAYS);
        assert!(usage.days.windows(2).all(|w| w[0].day < w[1].day));
        let today = on(&conn, &usage, t0);
        assert_eq!(today.generation_tokens, Some(300.0));
        // Other metrics were never observed: absent, not zero.
        assert_eq!(today.prompt_tokens, None);
    }

    #[test]
    fn restart_mid_day_counts_both_segments() {
        let conn = test_db();
        let t0 = now_ms() - 3_600_000;
        // 100 -> 500, restart (counter resets), 50 -> 200: 400 + 150 = 550.
        for (i, v) in [100.0, 500.0, 50.0, 200.0].iter().enumerate() {
            insert(
                &conn,
                t0 + i as i64 * 30_000,
                "a",
                "prompt_tokens_total",
                *v,
            );
        }
        let usage = query_on(&conn).unwrap();
        assert_eq!(on(&conn, &usage, t0).prompt_tokens, Some(550.0));
    }

    #[test]
    fn single_sample_partition_yields_absent_not_zero() {
        let conn = test_db();
        insert(&conn, now_ms() - 60_000, "a", "requests_total", 42.0);
        let usage = query_on(&conn).unwrap();
        // One sample -> no delta -> every axis day stays unobserved.
        assert!(usage.days.iter().all(|d| d.requests.is_none()));
    }

    #[test]
    fn idle_flat_counter_yields_zero_not_absent() {
        let conn = test_db();
        let t0 = now_ms() - 3_600_000;
        insert(&conn, t0, "a", "requests_total", 42.0);
        insert(&conn, t0 + 30_000, "a", "requests_total", 42.0);
        let usage = query_on(&conn).unwrap();
        assert_eq!(on(&conn, &usage, t0).requests, Some(0.0));
    }

    #[test]
    fn partitions_by_endpoint_so_fleet_totals_sum() {
        let conn = test_db();
        let t0 = now_ms() - 3_600_000;
        for (ep, base) in [("a", 0.0), ("b", 1_000.0)] {
            insert(&conn, t0, ep, "generation_tokens_total", base);
            insert(
                &conn,
                t0 + 30_000,
                ep,
                "generation_tokens_total",
                base + 10.0,
            );
        }
        let usage = query_on(&conn).unwrap();
        // Two endpoints, 10 tokens each: partitioned deltas, summed per day.
        assert_eq!(on(&conn, &usage, t0).generation_tokens, Some(20.0));
    }

    #[test]
    fn engine_null_partitions_do_not_mix_with_labeled_ones() {
        let conn = test_db();
        let t0 = now_ms() - 3_600_000;
        // Same endpoint/metric, one row set with engine NULL, one with '1':
        // they are independent counters and must not produce cross deltas.
        for (i, v) in [(0, 100.0), (1, 200.0)] {
            conn.execute(
                "INSERT INTO samples (ts_ms, endpoint, model, engine, metric, value)
                 VALUES (?1, 'a', 'm', NULL, 'requests_total', ?2)",
                rusqlite::params![t0 + i * 30_000, v],
            )
            .unwrap();
        }
        for (i, v) in [(0, 1_000.0), (1, 1_050.0)] {
            conn.execute(
                "INSERT INTO samples (ts_ms, endpoint, model, engine, metric, value)
                 VALUES (?1, 'a', 'm', '1', 'requests_total', ?2)",
                rusqlite::params![t0 + i * 30_000, v],
            )
            .unwrap();
        }
        let usage = query_on(&conn).unwrap();
        assert_eq!(on(&conn, &usage, t0).requests, Some(150.0));
    }

    #[test]
    fn window_excludes_days_older_than_30() {
        let conn = test_db();
        let old = now_ms() - 40 * 86_400_000;
        insert(&conn, old, "a", "requests_total", 1.0);
        insert(&conn, old + 30_000, "a", "requests_total", 5.0);
        let recent = now_ms() - 3_600_000;
        insert(&conn, recent, "a", "requests_total", 10.0);
        insert(&conn, recent + 30_000, "a", "requests_total", 12.0);
        let usage = query_on(&conn).unwrap();
        // The 40-day-old day is not even on the axis.
        assert!(!usage.days.iter().any(|d| d.day == local_day(&conn, old)));
        assert_eq!(on(&conn, &usage, recent).requests, Some(2.0));
    }

    #[test]
    fn chart_ring_metrics_are_ignored() {
        let conn = test_db();
        let t0 = now_ms() - 3_600_000;
        insert(&conn, t0, "a", "generation_tps", 50.0);
        insert(&conn, t0 + 30_000, "a", "generation_tps", 60.0);
        let usage = query_on(&conn).unwrap();
        assert!(
            usage
                .days
                .iter()
                .all(|d| d.generation_tokens.is_none() && d.requests.is_none())
        );
    }

    #[test]
    fn missing_database_is_a_clean_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = query_daily_usage(&dir.path().join("nope.db")).unwrap_err();
        assert!(err.contains("read-only"), "{err}");
    }

    #[test]
    fn query_while_recorder_writing_succeeds() {
        use crate::storage::{Recorder, SampleRow};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let recorder = Recorder::start(&path, 30).unwrap();
        let t0 = now_ms() - 3_600_000;
        recorder.record(vec![
            SampleRow {
                ts_ms: t0,
                endpoint: "a".into(),
                model: "m".into(),
                engine: Some("0".into()),
                metric: "prompt_tokens_total".into(),
                value: 100.0,
            },
            SampleRow {
                ts_ms: t0 + 30_000,
                endpoint: "a".into(),
                model: "m".into(),
                engine: Some("0".into()),
                metric: "prompt_tokens_total".into(),
                value: 400.0,
            },
        ]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while recorder.rows_written() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Reader on a separate connection while the writer thread lives.
        let usage = query_daily_usage(&path).unwrap();
        recorder.shutdown();
        let conn = test_db();
        assert_eq!(on(&conn, &usage, t0).prompt_tokens, Some(300.0));
    }
}

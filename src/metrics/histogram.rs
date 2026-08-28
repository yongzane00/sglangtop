//! Rolling percentile estimates from Prometheus histogram bucket deltas.
//!
//! Lifetime-cumulative buckets answer "since the server started"; we want
//! "recently". So we keep a short ring of cumulative snapshots and estimate
//! quantiles from the *delta* between the newest snapshot and the oldest one
//! within the window. Deltas of cumulative-in-`le` buckets are still
//! cumulative in `le`, so standard `histogram_quantile`-style linear
//! interpolation applies.
//!
//! All results are estimates (bucket resolution bounds accuracy) and are
//! labelled as such in the UI.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// One scrape's cumulative view of a histogram series.
#[derive(Debug, Clone, PartialEq)]
pub struct HistogramPoint {
    pub at: Instant,
    /// `(le, cumulative_count)` sorted ascending by `le`; `+Inf` last.
    pub buckets: Vec<(f64, f64)>,
    pub sum: f64,
    pub count: f64,
}

/// A histogram with more buckets than this is malformed or hostile; refusing
/// it keeps the snapshot ring's memory proportional to sane data
/// (vLLM's largest real histograms have ~25 buckets).
pub const MAX_BUCKETS: usize = 512;

impl HistogramPoint {
    /// Sort buckets and verify basic sanity (cumulative counts non-decreasing
    /// in `le`, at least one bucket, bounded bucket count). Returns None for
    /// malformed data.
    pub fn new(at: Instant, mut buckets: Vec<(f64, f64)>, sum: f64, count: f64) -> Option<Self> {
        if buckets.is_empty() || buckets.len() > MAX_BUCKETS {
            return None;
        }
        buckets.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        buckets.dedup_by(|a, b| a.0 == b.0);
        let mut prev = f64::NEG_INFINITY;
        let mut prev_count = 0.0;
        for &(le, c) in &buckets {
            if le.is_nan() || c.is_nan() || c < prev_count || le <= prev {
                return None;
            }
            prev = le;
            prev_count = c;
        }
        Some(HistogramPoint {
            at,
            buckets,
            sum,
            count,
        })
    }
}

/// Percentile estimates over one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindowEstimate {
    /// Actual elapsed time between the two snapshots used.
    pub elapsed: Duration,
    /// Number of observations that fell inside the window.
    pub observations: f64,
    /// Mean of windowed observations (sum delta / count delta), if computable.
    pub mean: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

/// Ring of cumulative snapshots for one histogram series.
#[derive(Debug)]
pub struct HistogramWindow {
    ring: VecDeque<HistogramPoint>,
    window: Duration,
}

/// Extra time beyond the window we keep points for, so a baseline point at or
/// before `now - window` is usually available.
const RETENTION_MARGIN: Duration = Duration::from_secs(15);
/// Hard cap on retained points regardless of scrape frequency.
const MAX_POINTS: usize = 1024;

impl HistogramWindow {
    pub fn new(window: Duration) -> Self {
        HistogramWindow {
            ring: VecDeque::new(),
            window,
        }
    }

    pub fn set_window(&mut self, window: Duration) {
        self.window = window;
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// Feed a new snapshot. Counter resets and bucket-layout changes clear
    /// the ring (history before a restart is not comparable).
    pub fn push(&mut self, point: HistogramPoint) {
        if let Some(last) = self.ring.back() {
            let layout_changed = last.buckets.len() != point.buckets.len()
                || last
                    .buckets
                    .iter()
                    .zip(point.buckets.iter())
                    .any(|(a, b)| a.0 != b.0);
            let reset = point.count < last.count;
            let non_monotonic_time = point.at <= last.at;
            if layout_changed || reset || non_monotonic_time {
                self.ring.clear();
            }
        }
        self.ring.push_back(point);
        self.evict();
    }

    fn evict(&mut self) {
        let Some(newest_at) = self.ring.back().map(|p| p.at) else {
            return;
        };
        let keep_horizon = self.window + RETENTION_MARGIN;
        while self.ring.len() > 2 {
            let second_oldest_at = self.ring[1].at;
            // Drop the oldest point only if the next one still covers the window.
            if newest_at.saturating_duration_since(second_oldest_at) >= self.window {
                self.ring.pop_front();
                continue;
            }
            let oldest_at = self.ring[0].at;
            if newest_at.saturating_duration_since(oldest_at) > keep_horizon {
                self.ring.pop_front();
                continue;
            }
            break;
        }
        while self.ring.len() > MAX_POINTS {
            self.ring.pop_front();
        }
    }

    /// Estimate windowed percentiles from oldest-retained vs newest snapshot.
    ///
    /// Returns None when fewer than two comparable snapshots exist. Returns
    /// an estimate with `observations == 0.0` and None percentiles when the
    /// window saw no new observations (that is a real "no traffic" signal,
    /// distinct from "cannot compute").
    pub fn estimate(&self) -> Option<WindowEstimate> {
        let newest = self.ring.back()?;
        let oldest = self.ring.front()?;
        if std::ptr::eq(newest, oldest) || newest.at <= oldest.at {
            return None;
        }
        let elapsed = newest.at.saturating_duration_since(oldest.at);

        // Per-bucket deltas; layout equality is guaranteed by push().
        let mut deltas: Vec<(f64, f64)> = Vec::with_capacity(newest.buckets.len());
        for (&(le, new_c), &(_, old_c)) in newest.buckets.iter().zip(oldest.buckets.iter()) {
            let d = new_c - old_c;
            if d < 0.0 {
                return None; // shouldn't happen post-push checks; be safe
            }
            deltas.push((le, d));
        }
        let total = deltas.last().map(|&(_, c)| c).unwrap_or(0.0);
        let count_delta = newest.count - oldest.count;
        let sum_delta = newest.sum - oldest.sum;
        if total <= 0.0 {
            return Some(WindowEstimate {
                elapsed,
                observations: 0.0,
                mean: None,
                p50: None,
                p95: None,
                p99: None,
            });
        }
        let mean = if count_delta > 0.0 && sum_delta.is_finite() {
            Some(sum_delta / count_delta)
        } else {
            None
        };
        Some(WindowEstimate {
            elapsed,
            observations: total,
            mean,
            p50: quantile_from_cumulative(&deltas, 0.50),
            p95: quantile_from_cumulative(&deltas, 0.95),
            p99: quantile_from_cumulative(&deltas, 0.99),
        })
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// `histogram_quantile`-style estimation over cumulative-in-`le` counts.
///
/// - The lowest bucket interpolates from a lower bound of 0 (or `le` itself
///   when `le <= 0`, mirroring Prometheus).
/// - If the quantile lands in the `+Inf` bucket, the highest finite bound is
///   returned (the estimate saturates).
fn quantile_from_cumulative(buckets: &[(f64, f64)], q: f64) -> Option<f64> {
    let total = buckets.last().map(|&(_, c)| c)?;
    if total <= 0.0 || !(0.0..=1.0).contains(&q) {
        return None;
    }
    let target = q * total;
    let mut prev_le = 0.0_f64;
    let mut prev_count = 0.0_f64;
    let mut highest_finite = None;
    for &(le, cum) in buckets {
        if le.is_finite() {
            highest_finite = Some(le);
        }
        if cum >= target {
            if !le.is_finite() {
                return highest_finite;
            }
            let bucket_count = cum - prev_count;
            if bucket_count <= 0.0 {
                return Some(le);
            }
            let lower = if prev_count == 0.0 && buckets.first().map(|&(l, _)| l) == Some(le) {
                if le > 0.0 { 0.0 } else { le }
            } else {
                prev_le
            };
            let frac = (target - prev_count) / bucket_count;
            return Some(lower + (le - lower) * frac);
        }
        prev_le = le;
        prev_count = cum;
    }
    highest_finite
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    fn point(at: Instant, buckets: &[(f64, f64)], sum: f64, count: f64) -> HistogramPoint {
        HistogramPoint::new(at, buckets.to_vec(), sum, count).expect("valid point")
    }

    const LE: [f64; 4] = [0.1, 1.0, 10.0, f64::INFINITY];

    fn cum(at: Instant, counts: [f64; 4], sum: f64) -> HistogramPoint {
        let buckets: Vec<(f64, f64)> = LE.iter().copied().zip(counts).collect();
        let count = counts[3];
        point(at, &buckets, sum, count)
    }

    #[test]
    fn single_point_gives_no_estimate() {
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        w.push(cum(Instant::now(), [1.0, 2.0, 3.0, 3.0], 5.0));
        assert!(w.estimate().is_none());
    }

    #[test]
    fn quantiles_interpolate_within_bucket() {
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        w.push(cum(t(base, 0.0), [0.0, 0.0, 0.0, 0.0], 0.0));
        // 100 new observations, all in (0.1, 1.0].
        w.push(cum(t(base, 10.0), [0.0, 100.0, 100.0, 100.0], 55.0));
        let e = w.estimate().unwrap();
        assert_eq!(e.observations, 100.0);
        // p50 = 0.1 + 0.5*(1.0-0.1) = 0.55
        assert!((e.p50.unwrap() - 0.55).abs() < 1e-9);
        assert!((e.p95.unwrap() - 0.955).abs() < 1e-9);
        assert_eq!(e.mean, Some(0.55));
        assert_eq!(e.elapsed, Duration::from_secs(10));
    }

    #[test]
    fn lowest_bucket_interpolates_from_zero() {
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        w.push(cum(t(base, 0.0), [0.0, 0.0, 0.0, 0.0], 0.0));
        w.push(cum(t(base, 1.0), [10.0, 10.0, 10.0, 10.0], 0.5));
        let e = w.estimate().unwrap();
        // All mass in [0, 0.1]: p50 = 0.05.
        assert!((e.p50.unwrap() - 0.05).abs() < 1e-9);
    }

    #[test]
    fn quantile_in_inf_bucket_saturates_to_highest_finite_bound() {
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        w.push(cum(t(base, 0.0), [0.0, 0.0, 0.0, 0.0], 0.0));
        // 40% of mass beyond the last finite bucket.
        w.push(cum(t(base, 1.0), [0.0, 0.0, 60.0, 100.0], 0.0));
        let e = w.estimate().unwrap();
        assert_eq!(e.p95, Some(10.0));
        assert_eq!(e.p99, Some(10.0));
    }

    #[test]
    fn no_new_observations_is_zero_not_unavailable() {
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        w.push(cum(t(base, 0.0), [5.0, 6.0, 7.0, 7.0], 3.0));
        w.push(cum(t(base, 10.0), [5.0, 6.0, 7.0, 7.0], 3.0));
        let e = w.estimate().unwrap();
        assert_eq!(e.observations, 0.0);
        assert_eq!(e.p50, None);
        assert_eq!(e.mean, None);
    }

    #[test]
    fn counter_reset_clears_history() {
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        w.push(cum(t(base, 0.0), [50.0, 60.0, 70.0, 70.0], 100.0));
        w.push(cum(t(base, 10.0), [55.0, 66.0, 77.0, 77.0], 110.0));
        assert_eq!(w.len(), 2);
        // Restart: counts fall.
        w.push(cum(t(base, 20.0), [1.0, 2.0, 3.0, 3.0], 1.0));
        assert_eq!(w.len(), 1);
        assert!(w.estimate().is_none());
        // Recovers on the following scrape.
        w.push(cum(t(base, 30.0), [2.0, 4.0, 6.0, 6.0], 2.0));
        assert!(w.estimate().is_some());
    }

    #[test]
    fn bucket_layout_change_clears_history() {
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        w.push(cum(t(base, 0.0), [1.0, 2.0, 3.0, 3.0], 0.0));
        let other = point(t(base, 10.0), &[(0.5, 4.0), (f64::INFINITY, 5.0)], 0.0, 5.0);
        w.push(other);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn eviction_keeps_window_coverage() {
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        for i in 0..200 {
            let v = i as f64;
            w.push(cum(t(base, v), [v, v, v, v], v));
        }
        // Ring stays bounded but still covers >= the window.
        assert!(w.len() < 100, "ring should be bounded, was {}", w.len());
        let e = w.estimate().unwrap();
        assert!(e.elapsed >= Duration::from_secs(60));
    }

    #[test]
    fn malformed_points_rejected() {
        let now = Instant::now();
        // Cumulative counts must be non-decreasing in le.
        assert!(
            HistogramPoint::new(
                now,
                vec![(0.1, 10.0), (1.0, 5.0), (f64::INFINITY, 12.0)],
                0.0,
                12.0
            )
            .is_none()
        );
        assert!(HistogramPoint::new(now, vec![], 0.0, 0.0).is_none());
        // Absurd bucket cardinality rejected (memory bound).
        let huge: Vec<(f64, f64)> = (0..=MAX_BUCKETS).map(|i| (i as f64, 0.0)).collect();
        assert!(HistogramPoint::new(now, huge, 0.0, 0.0).is_none());
        // NaN le rejected.
        assert!(
            HistogramPoint::new(now, vec![(f64::NAN, 1.0), (f64::INFINITY, 1.0)], 0.0, 1.0)
                .is_none()
        );
    }

    #[test]
    fn missing_middle_bucket_still_estimates() {
        // A server may omit some buckets; layout is whatever it exposes.
        let base = Instant::now();
        let mut w = HistogramWindow::new(Duration::from_secs(60));
        let mk = |at, a: f64, b: f64| point(at, &[(1.0, a), (f64::INFINITY, b)], 0.0, b);
        w.push(mk(t(base, 0.0), 0.0, 0.0));
        w.push(mk(t(base, 5.0), 8.0, 10.0));
        let e = w.estimate().unwrap();
        assert!(e.p50.is_some());
    }
}

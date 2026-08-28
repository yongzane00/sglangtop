//! Time-windowed ring buffers for in-memory metric history.
//!
//! Memory is bounded two ways: by time (points older than the window are
//! evicted relative to the newest point) and by a hard point cap (so a
//! misconfigured sub-second refresh interval cannot grow memory without
//! bound).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Hard cap on points per series regardless of refresh interval.
pub const MAX_POINTS_PER_SERIES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimePoint {
    pub at: Instant,
    pub value: f64,
}

/// A single series' rolling history.
#[derive(Debug, Clone)]
pub struct RingSeries {
    points: VecDeque<TimePoint>,
    window: Duration,
}

impl RingSeries {
    pub fn new(window: Duration) -> Self {
        RingSeries {
            points: VecDeque::new(),
            window,
        }
    }

    pub fn set_window(&mut self, window: Duration) {
        self.window = window;
        self.evict();
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    pub fn push(&mut self, at: Instant, value: f64) {
        // Ignore time going backwards (should not happen with Instant).
        if let Some(last) = self.points.back()
            && at < last.at
        {
            return;
        }
        self.points.push_back(TimePoint { at, value });
        self.evict();
    }

    fn evict(&mut self) {
        let Some(newest) = self.points.back().map(|p| p.at) else {
            return;
        };
        while let Some(front) = self.points.front() {
            if newest.saturating_duration_since(front.at) > self.window {
                self.points.pop_front();
            } else {
                break;
            }
        }
        while self.points.len() > MAX_POINTS_PER_SERIES {
            self.points.pop_front();
        }
    }

    pub fn latest(&self) -> Option<TimePoint> {
        self.points.back().copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = TimePoint> + '_ {
        self.points.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// Min/max over the retained points (for chart axis scaling). NaN points
    /// are skipped.
    pub fn value_bounds(&self) -> Option<(f64, f64)> {
        let mut bounds: Option<(f64, f64)> = None;
        for p in &self.points {
            if p.value.is_nan() {
                continue;
            }
            bounds = Some(match bounds {
                None => (p.value, p.value),
                Some((lo, hi)) => (lo.min(p.value), hi.max(p.value)),
            });
        }
        bounds
    }

    /// The most recent `n` values, oldest first (for sparklines).
    pub fn tail_values(&self, n: usize) -> Vec<f64> {
        let skip = self.points.len().saturating_sub(n);
        self.points.iter().skip(skip).map(|p| p.value).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn evicts_by_time_window() {
        let base = Instant::now();
        let mut s = RingSeries::new(Duration::from_secs(10));
        for i in 0..30 {
            s.push(t(base, i), i as f64);
        }
        // Newest at t=29; only points within [19, 29] retained.
        assert_eq!(s.len(), 11);
        assert_eq!(s.iter().next().unwrap().value, 19.0);
        assert_eq!(s.latest().unwrap().value, 29.0);
    }

    #[test]
    fn hard_cap_bounds_memory() {
        let base = Instant::now();
        let mut s = RingSeries::new(Duration::from_secs(1_000_000));
        for i in 0..(MAX_POINTS_PER_SERIES as u64 + 500) {
            s.push(base + Duration::from_millis(i), i as f64);
        }
        assert_eq!(s.len(), MAX_POINTS_PER_SERIES);
    }

    #[test]
    fn shrinking_window_evicts_immediately() {
        let base = Instant::now();
        let mut s = RingSeries::new(Duration::from_secs(100));
        for i in 0..50 {
            s.push(t(base, i), i as f64);
        }
        assert_eq!(s.len(), 50);
        s.set_window(Duration::from_secs(5));
        assert_eq!(s.len(), 6);
    }

    #[test]
    fn out_of_order_pushes_ignored() {
        let base = Instant::now();
        let mut s = RingSeries::new(Duration::from_secs(100));
        s.push(t(base, 10), 1.0);
        s.push(t(base, 5), 2.0);
        assert_eq!(s.len(), 1);
        assert_eq!(s.latest().unwrap().value, 1.0);
    }

    #[test]
    fn bounds_skip_nan() {
        let base = Instant::now();
        let mut s = RingSeries::new(Duration::from_secs(100));
        s.push(t(base, 0), 5.0);
        s.push(t(base, 1), f64::NAN);
        s.push(t(base, 2), -3.0);
        assert_eq!(s.value_bounds(), Some((-3.0, 5.0)));
    }

    #[test]
    fn tail_values_oldest_first() {
        let base = Instant::now();
        let mut s = RingSeries::new(Duration::from_secs(100));
        for i in 0..10 {
            s.push(t(base, i), i as f64);
        }
        assert_eq!(s.tail_values(3), vec![7.0, 8.0, 9.0]);
        assert_eq!(s.tail_values(100).len(), 10);
    }
}

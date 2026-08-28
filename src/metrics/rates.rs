//! Reset-aware interval rates from monotonically scraped counters.
//!
//! Rules (see docs/PLAN.md):
//! - Elapsed time comes from a monotonic clock (`Instant`), never wall time.
//! - A negative delta means the counter reset (vLLM restart): the rate is
//!   *unavailable* for that interval, never negative.
//! - The first sample after startup or a reset yields no rate.
//! - NaN/Inf poison the tracker: state resets, no rate is produced.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::Instant;

/// Outcome of feeding one counter observation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateSample {
    /// First sample ever, or first after a reset/poison: no interval yet.
    Unavailable,
    /// Counter went backwards: the source restarted. No rate this interval.
    Reset,
    /// A valid interval.
    Rate {
        per_sec: f64,
        /// Raw increase over the interval (useful for "new errors" flags).
        delta: f64,
    },
}

impl RateSample {
    pub fn per_sec(self) -> Option<f64> {
        match self {
            RateSample::Rate { per_sec, .. } => Some(per_sec),
            _ => None,
        }
    }

    pub fn delta(self) -> Option<f64> {
        match self {
            RateSample::Rate { delta, .. } => Some(delta),
            _ => None,
        }
    }

    pub fn is_reset(self) -> bool {
        matches!(self, RateSample::Reset)
    }
}

/// Tracks one counter series.
#[derive(Debug, Default, Clone)]
pub struct CounterTracker {
    last: Option<(Instant, f64)>,
}

impl CounterTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, now: Instant, value: f64) -> RateSample {
        if !value.is_finite() {
            self.last = None;
            return RateSample::Unavailable;
        }
        let prev = self.last.replace((now, value));
        let Some((t0, v0)) = prev else {
            return RateSample::Unavailable;
        };
        let dv = value - v0;
        if dv < 0.0 {
            return RateSample::Reset;
        }
        let dt = now.saturating_duration_since(t0).as_secs_f64();
        if dt <= 0.0 {
            return RateSample::Unavailable;
        }
        RateSample::Rate {
            per_sec: dv / dt,
            delta: dv,
        }
    }
}

/// A bank of counter trackers keyed by caller-chosen series identity
/// (e.g. `(metric name, label set)`), with generation-based eviction so label
/// churn cannot grow memory forever.
#[derive(Debug)]
pub struct CounterBank<K: Eq + Hash> {
    series: HashMap<K, Entry>,
    generation: u64,
}

impl<K: Eq + Hash> Default for CounterBank<K> {
    fn default() -> Self {
        CounterBank {
            series: HashMap::new(),
            generation: 0,
        }
    }
}

#[derive(Debug)]
struct Entry {
    tracker: CounterTracker,
    last_seen: u64,
}

/// Series absent for this many scrapes get evicted.
const EVICT_AFTER_GENERATIONS: u64 = 16;

impl<K: Eq + Hash> CounterBank<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Call once per scrape before feeding samples.
    pub fn begin_scrape(&mut self) {
        self.generation += 1;
    }

    pub fn update(&mut self, key: K, now: Instant, value: f64) -> RateSample {
        let generation = self.generation;
        let entry = self.series.entry(key).or_insert_with(|| Entry {
            tracker: CounterTracker::new(),
            last_seen: generation,
        });
        entry.last_seen = generation;
        entry.tracker.update(now, value)
    }

    /// Call once per scrape after feeding samples; drops stale series.
    pub fn end_scrape(&mut self) {
        let generation = self.generation;
        self.series
            .retain(|_, e| generation.saturating_sub(e.last_seen) < EVICT_AFTER_GENERATIONS);
    }

    pub fn series_count(&self) -> usize {
        self.series.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(base: Instant, secs: f64) -> Instant {
        base + Duration::from_secs_f64(secs)
    }

    #[test]
    fn first_sample_is_unavailable() {
        let mut c = CounterTracker::new();
        assert_eq!(c.update(Instant::now(), 100.0), RateSample::Unavailable);
    }

    #[test]
    fn steady_rate() {
        let base = Instant::now();
        let mut c = CounterTracker::new();
        c.update(t(base, 0.0), 1000.0);
        let s = c.update(t(base, 2.0), 1500.0);
        assert_eq!(
            s,
            RateSample::Rate {
                per_sec: 250.0,
                delta: 500.0
            }
        );
    }

    #[test]
    fn zero_delta_is_zero_rate_not_unavailable() {
        let base = Instant::now();
        let mut c = CounterTracker::new();
        c.update(t(base, 0.0), 42.0);
        assert_eq!(c.update(t(base, 1.0), 42.0).per_sec(), Some(0.0));
    }

    #[test]
    fn reset_is_flagged_and_never_negative() {
        let base = Instant::now();
        let mut c = CounterTracker::new();
        c.update(t(base, 0.0), 5000.0);
        let s = c.update(t(base, 1.0), 12.0); // restart: counter fell
        assert!(s.is_reset());
        assert_eq!(s.per_sec(), None);
        // Next interval works again, measured from the post-reset value.
        let s = c.update(t(base, 3.0), 212.0);
        assert_eq!(s.per_sec(), Some(100.0));
    }

    #[test]
    fn same_instant_gives_no_rate() {
        let base = Instant::now();
        let mut c = CounterTracker::new();
        c.update(base, 1.0);
        assert_eq!(c.update(base, 2.0), RateSample::Unavailable);
    }

    #[test]
    fn nan_poisons_then_recovers() {
        let base = Instant::now();
        let mut c = CounterTracker::new();
        c.update(t(base, 0.0), 10.0);
        assert_eq!(c.update(t(base, 1.0), f64::NAN), RateSample::Unavailable);
        // The next real sample is treated as a fresh first sample.
        assert_eq!(c.update(t(base, 2.0), 20.0), RateSample::Unavailable);
        assert_eq!(c.update(t(base, 3.0), 30.0).per_sec(), Some(10.0));
    }

    #[test]
    fn bank_tracks_series_independently_and_evicts() {
        let base = Instant::now();
        let mut bank: CounterBank<(&str, &str)> = CounterBank::new();

        bank.begin_scrape();
        bank.update(("m", "a"), t(base, 0.0), 0.0);
        bank.update(("m", "b"), t(base, 0.0), 0.0);
        bank.end_scrape();
        assert_eq!(bank.series_count(), 2);

        bank.begin_scrape();
        assert_eq!(
            bank.update(("m", "a"), t(base, 1.0), 10.0).per_sec(),
            Some(10.0)
        );
        bank.end_scrape();

        // Series b disappears; after enough generations it is evicted.
        for i in 0..20 {
            bank.begin_scrape();
            bank.update(("m", "a"), t(base, 2.0 + i as f64), 10.0);
            bank.end_scrape();
        }
        assert_eq!(bank.series_count(), 1);
    }
}

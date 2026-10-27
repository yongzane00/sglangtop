//! Prompt bursts: the whole prompts of the requests that finished within
//! one scrape interval.
//!
//! SGLang adds a request's *entire* prompt length to
//! `sglang:prompt_tokens_total` and increments `sglang:num_requests_total`
//! in the same call (`observe_one_finished_request`), once, as the request
//! finishes. So over one scrape interval the prompt counter's delta is the
//! sum of the prompts of the requests that finished, and the request
//! counter's delta is how many they were. One request finishing alone in
//! an interval therefore yields its exact prompt size; several finishing
//! together share one burst, which the UI labels as such rather than
//! guessing a split.
//!
//! The ring holds the history window and nothing more; nothing here is
//! recorded to SQLite.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Hard cap on entries, so a server that lands prompts on every scrape for
/// an hour-long window still stays bounded.
pub const MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone, PartialEq)]
pub struct Burst {
    /// The scrape that first showed the tokens: the requests finished in
    /// the interval ending here.
    pub at: Instant,
    /// Prompt tokens that landed, summed across engines and models.
    pub tokens: f64,
    /// How many requests they were (`Δ` of `num_requests_total`), `None`
    /// when some series cannot say.
    pub requests: Option<u32>,
}

/// Newest at the back; the renderer iterates newest-first.
#[derive(Debug)]
pub struct BurstLog {
    entries: VecDeque<Burst>,
    window: Duration,
}

impl BurstLog {
    pub fn new(window: Duration) -> Self {
        BurstLog {
            entries: VecDeque::new(),
            window,
        }
    }

    /// Record one interval's delta. Intervals without prompt tokens are not
    /// bursts and are skipped; a zero request count is "unknown", since
    /// prompt tokens cannot be counted without a request finishing.
    pub fn push(&mut self, at: Instant, tokens: f64, requests: Option<f64>) {
        if !tokens.is_finite() || tokens <= 0.0 {
            return;
        }
        let requests = requests
            .filter(|n| n.is_finite() && *n >= 0.5)
            .map(|n| n.round().min(f64::from(u32::MAX)) as u32);
        self.entries.push_back(Burst {
            at,
            tokens,
            requests,
        });
        self.evict(at);
    }

    fn evict(&mut self, now: Instant) {
        while let Some(front) = self.entries.front() {
            let old = now.saturating_duration_since(front.at) > self.window;
            if old || self.entries.len() > MAX_ENTRIES {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    /// Bursts still inside the window as of `now`, newest first.
    pub fn recent(&self, now: Instant) -> impl Iterator<Item = &Burst> {
        self.entries
            .iter()
            .rev()
            .take_while(move |b| now.saturating_duration_since(b.at) <= self.window)
    }

    /// Prompt tokens that landed inside the window.
    pub fn total_tokens(&self, now: Instant) -> f64 {
        self.recent(now).map(|b| b.tokens).sum()
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

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn only_positive_token_deltas_become_bursts() {
        let base = Instant::now();
        let mut log = BurstLog::new(secs(300));
        log.push(base, 0.0, Some(1.0));
        log.push(base, -5.0, Some(1.0));
        log.push(base, f64::NAN, Some(1.0));
        assert!(log.is_empty());
        log.push(base + secs(1), 55_200.0, Some(1.0));
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn request_counts_round_and_zero_means_unknown() {
        let base = Instant::now();
        let mut log = BurstLog::new(secs(300));
        log.push(base, 10.0, Some(0.0));
        log.push(base + secs(1), 10.0, Some(2.4));
        log.push(base + secs(2), 10.0, None);
        let got: Vec<Option<u32>> = log.recent(base + secs(2)).map(|b| b.requests).collect();
        assert_eq!(got, vec![None, Some(2), None]);
    }

    #[test]
    fn the_window_bounds_what_is_kept_and_what_is_read() {
        let base = Instant::now();
        let mut log = BurstLog::new(secs(60));
        log.push(base, 100.0, Some(1.0));
        log.push(base + secs(30), 200.0, Some(1.0));
        log.push(base + secs(61), 300.0, Some(1.0));
        // The first burst fell out of the window on the last push.
        assert_eq!(log.len(), 2);
        // Reading later than the last push narrows the view further.
        let now = base + secs(95);
        let seen: Vec<f64> = log.recent(now).map(|b| b.tokens).collect();
        assert_eq!(seen, vec![300.0]);
        assert_eq!(log.total_tokens(now), 300.0);
        assert_eq!(log.total_tokens(base + secs(61)), 500.0);
    }

    #[test]
    fn the_entry_cap_holds_under_a_prompt_every_scrape() {
        let base = Instant::now();
        let mut log = BurstLog::new(secs(3600 * 24));
        for i in 0..(MAX_ENTRIES as u64 + 50) {
            log.push(base + Duration::from_millis(i), 1.0, Some(1.0));
        }
        assert_eq!(log.len(), MAX_ENTRIES);
    }
}

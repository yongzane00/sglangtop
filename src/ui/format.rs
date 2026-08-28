//! Value formatting: compact, fixed-width-friendly, honest about
//! unavailability (`--`, never a fake zero) and specials (NaN/Inf shown as
//! such in raw views).

use std::time::{Duration, Instant};

pub const NA: &str = "--";

/// Compact count: 950 → "950", 12_345 → "12.3k", 3.7e6 → "3.70M".
pub fn count(v: Option<f64>) -> String {
    match v {
        None => NA.into(),
        Some(v) if !v.is_finite() => raw_value(v),
        Some(v) => {
            let a = v.abs();
            if a >= 1e12 {
                format!("{:.2}T", v / 1e12)
            } else if a >= 1e9 {
                format!("{:.2}G", v / 1e9)
            } else if a >= 1e6 {
                format!("{:.2}M", v / 1e6)
            } else if a >= 10_000.0 {
                format!("{:.1}k", v / 1e3)
            } else if v.fract() == 0.0 {
                format!("{v:.0}")
            } else {
                format!("{v:.1}")
            }
        }
    }
}

/// Rate with unit suffix: "1.2k/s".
pub fn rate(v: Option<f64>) -> String {
    match v {
        None => NA.into(),
        Some(v) => format!("{}/s", count(Some(v))),
    }
}

/// Seconds with adaptive precision: 0.0042 → "4.2ms", 1.234 → "1.23s",
/// 95.0 → "1m35s".
pub fn seconds(v: Option<f64>) -> String {
    match v {
        None => NA.into(),
        Some(v) if !v.is_finite() || v < 0.0 => raw_value(v),
        Some(v) if v < 0.001 => format!("{:.0}µs", v * 1e6),
        Some(v) if v < 1.0 => format!("{:.0}ms", v * 1e3),
        Some(v) if v < 10.0 => format!("{v:.2}s"),
        Some(v) if v < 60.0 => format!("{v:.1}s"),
        Some(v) => {
            let total = v.round() as u64;
            let (m, s) = (total / 60, total % 60);
            if m < 60 {
                format!("{m}m{s:02}s")
            } else {
                format!("{}h{:02}m", m / 60, m % 60)
            }
        }
    }
}

/// Percent from a 0..=1 fraction: "42.0%".
pub fn percent(v: Option<f64>) -> String {
    match v {
        None => NA.into(),
        Some(v) if !v.is_finite() => raw_value(v),
        Some(v) => format!("{:.1}%", v * 100.0),
    }
}

/// Elapsed-since formatting for "last scrape" ages.
pub fn ago(at: Option<Instant>, now: Instant) -> String {
    match at {
        None => "never".into(),
        Some(at) => {
            let d = now.saturating_duration_since(at);
            if d < Duration::from_millis(1500) {
                "now".into()
            } else {
                format!("{} ago", brief_duration(d))
            }
        }
    }
}

pub fn brief_duration(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 60.0 {
        format!("{s:.0}s")
    } else if s < 3600.0 {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    } else {
        format!("{}h{:02}m", d.as_secs() / 3600, (d.as_secs() % 3600) / 60)
    }
}

/// Raw metric value for the Raw Metrics view: preserves NaN/±Inf spellings
/// and full precision for normal numbers.
pub fn raw_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".into()
    } else if v == f64::INFINITY {
        "+Inf".into()
    } else if v == f64::NEG_INFINITY {
        "-Inf".into()
    } else if v == 0.0 {
        "0".into()
    } else if v.abs() >= 1e15 || v.abs() < 1e-6 {
        format!("{v:.6e}")
    } else if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

/// Unicode utilization bar of exactly `width` cells using eighth blocks.
/// `ascii` mode uses '#'/'.' for terminals without good Unicode fonts.
pub fn bar(frac: f64, width: usize, ascii: bool) -> String {
    let frac = frac.clamp(0.0, 1.0);
    if width == 0 {
        return String::new();
    }
    if ascii {
        let filled = (frac * width as f64).round() as usize;
        return format!("{}{}", "#".repeat(filled), ".".repeat(width - filled));
    }
    const PARTIALS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
    let cells = frac * width as f64;
    let full = cells.floor() as usize;
    let remainder = cells - full as f64;
    let mut out = String::with_capacity(width * 3);
    for _ in 0..full.min(width) {
        out.push('█');
    }
    if full < width {
        let idx = (remainder * 8.0).floor() as usize;
        if idx > 0 {
            out.push(PARTIALS[idx.min(7) - 1]);
        } else {
            out.push(' ');
        }
        for _ in (full + 1)..width {
            out.push(' ');
        }
    }
    out
}

/// Inline sparkline string using ▁▂▃▄▅▆▇█, scaled to the slice's own range.
/// Takes the most recent `width` values (oldest first).
pub fn spark(values: &[f64], width: usize, ascii: bool) -> String {
    if width == 0 {
        return String::new();
    }
    let take = values.len().min(width);
    let slice = &values[values.len() - take..];
    let finite: Vec<f64> = slice.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return " ".repeat(width);
    }
    let lo = finite.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let levels: &[char] = if ascii {
        &['_', '.', '-', '=', '+', '*', '%', '#']
    } else {
        &['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█']
    };
    let mut out = String::with_capacity(width * 3);
    // Left-pad so the newest value is always at the right edge.
    for _ in 0..(width - take) {
        out.push(' ');
    }
    for &v in slice {
        if !v.is_finite() {
            out.push(' ');
            continue;
        }
        let idx = if hi > lo {
            (((v - lo) / (hi - lo)) * 7.0).round() as usize
        } else {
            0
        };
        out.push(levels[idx.min(7)]);
    }
    out
}

/// Truncate to `max` display cells, appending '…' when cut. ASCII-safe for
/// the character widths we render (metric names/labels).
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_formats() {
        assert_eq!(count(None), "--");
        assert_eq!(count(Some(0.0)), "0");
        assert_eq!(count(Some(950.0)), "950");
        assert_eq!(count(Some(2.5)), "2.5");
        assert_eq!(count(Some(12_345.0)), "12.3k");
        assert_eq!(count(Some(3_700_000.0)), "3.70M");
        assert_eq!(count(Some(9_999.0)), "9999");
    }

    #[test]
    fn seconds_formats() {
        assert_eq!(seconds(None), "--");
        assert_eq!(seconds(Some(0.0042)), "4ms");
        assert_eq!(seconds(Some(0.000_42)), "420µs");
        assert_eq!(seconds(Some(1.234)), "1.23s");
        assert_eq!(seconds(Some(42.4)), "42.4s");
        assert_eq!(seconds(Some(95.0)), "1m35s");
        assert_eq!(seconds(Some(7_320.0)), "2h02m");
    }

    #[test]
    fn percent_formats() {
        assert_eq!(percent(Some(0.42)), "42.0%");
        assert_eq!(percent(None), "--");
    }

    #[test]
    fn raw_preserves_specials() {
        assert_eq!(raw_value(f64::NAN), "NaN");
        assert_eq!(raw_value(f64::INFINITY), "+Inf");
        assert_eq!(raw_value(f64::NEG_INFINITY), "-Inf");
        assert_eq!(raw_value(3.763908e6), "3763908");
        assert_eq!(raw_value(0.25), "0.25");
    }

    #[test]
    fn bar_geometry() {
        assert_eq!(bar(0.0, 4, false).chars().count(), 4);
        assert_eq!(bar(0.5, 4, false).chars().count(), 4);
        assert_eq!(bar(1.0, 4, false), "████");
        assert_eq!(bar(2.0, 4, false), "████"); // clamped
        assert_eq!(bar(0.5, 4, true), "##..");
    }

    #[test]
    fn spark_right_aligned_and_scaled() {
        let s = spark(&[0.0, 5.0, 10.0], 5, false);
        assert_eq!(s.chars().count(), 5);
        assert!(s.starts_with("  "));
        assert!(s.ends_with('█'));
        // Flat series stays at the floor, not mid-height.
        let flat = spark(&[3.0, 3.0, 3.0], 3, false);
        assert_eq!(flat, "▁▁▁");
        // Empty/NaN-only input renders blanks, not a panic.
        assert_eq!(spark(&[], 3, false), "   ");
        assert_eq!(spark(&[f64::NAN], 3, false), "   ");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("vllm:num_requests_running", 12), "vllm:num_re…");
    }

    #[test]
    fn ago_formats() {
        let now = Instant::now();
        assert_eq!(ago(None, now), "never");
        assert_eq!(ago(Some(now), now), "now");
        let earlier = now - Duration::from_secs(83);
        assert_eq!(ago(Some(earlier), now), "1m23s ago");
    }
}

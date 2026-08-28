//! Core data types for scraped Prometheus metrics.
//!
//! These types are produced by [`crate::metrics::parse`] and consumed by the
//! normalization/state layers. They deliberately preserve *everything* the
//! endpoint exposed (unknown families included) so curation can evolve
//! without reparsing.

use std::fmt;

/// Prometheus metric family type, from `# TYPE` lines.
///
/// The type must come from metadata, never from name suffixes: vLLM exposes
/// `vllm:iteration_tokens_total` as a *histogram* despite the `_total` suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricType {
    Counter,
    Gauge,
    Histogram,
    Summary,
    /// No `# TYPE` line was seen for this family.
    Untyped,
}

impl MetricType {
    pub fn as_str(self) -> &'static str {
        match self {
            MetricType::Counter => "counter",
            MetricType::Gauge => "gauge",
            MetricType::Histogram => "histogram",
            MetricType::Summary => "summary",
            MetricType::Untyped => "untyped",
        }
    }
}

impl fmt::Display for MetricType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of labels, stored sorted by label name for a stable identity.
///
/// Sorting makes `LabelSet` usable as a series key: two samples with the same
/// labels in different textual order compare equal.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct LabelSet(Vec<(String, String)>);

impl LabelSet {
    pub fn new(mut pairs: Vec<(String, String)>) -> Self {
        pairs.sort();
        LabelSet(pairs)
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// A copy of this label set without the named labels (e.g. drop `le` when
    /// grouping histogram buckets into one series).
    pub fn without(&self, names: &[&str]) -> LabelSet {
        LabelSet(
            self.0
                .iter()
                .filter(|(k, _)| !names.contains(&k.as_str()))
                .cloned()
                .collect(),
        )
    }
}

impl fmt::Display for LabelSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            return Ok(());
        }
        f.write_str("{")?;
        for (i, (k, v)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            write!(f, "{k}=\"{v}\"")?;
        }
        f.write_str("}")
    }
}

/// One sample line from the exposition text.
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// The full sample name as it appeared (`vllm:request_prompt_tokens_bucket`).
    pub name: String,
    pub labels: LabelSet,
    /// Values are always f64 in the text format; NaN and ±Inf are representable.
    pub value: f64,
    /// Optional trailing timestamp, milliseconds since epoch.
    pub timestamp_ms: Option<i64>,
}

/// A metric family: metadata plus all its samples.
///
/// For histograms/summaries the family name is the base name from `# TYPE`
/// (`vllm:ttft_seconds`) while samples keep their suffixed names
/// (`vllm:ttft_seconds_bucket`, `_sum`, `_count`).
#[derive(Debug, Clone, PartialEq)]
pub struct MetricFamily {
    pub name: String,
    pub help: Option<String>,
    pub kind: MetricType,
    pub samples: Vec<Sample>,
}

/// A non-fatal problem encountered while parsing; the offending line is
/// skipped and reported instead of failing the whole scrape.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseIssue {
    pub line: usize,
    pub message: String,
}

/// The result of parsing one exposition document.
#[derive(Debug, Clone, Default)]
pub struct ScrapeText {
    /// Families in first-seen order (deterministic display).
    pub families: Vec<MetricFamily>,
    /// The first [`crate::metrics::parse::MAX_PARSE_ISSUES`] issues only;
    /// `issue_count` is the honest total.
    pub issues: Vec<ParseIssue>,
    /// Every issue encountered, including those not stored in `issues`.
    pub issue_count: usize,
}

impl ScrapeText {
    pub fn family(&self, name: &str) -> Option<&MetricFamily> {
        self.families.iter().find(|f| f.name == name)
    }

    pub fn total_samples(&self) -> usize {
        self.families.iter().map(|f| f.samples.len()).sum()
    }
}

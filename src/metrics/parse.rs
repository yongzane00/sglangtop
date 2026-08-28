//! A small, strict-where-it-matters Prometheus text-format parser.
//!
//! Why not a crate: the exposition text vLLM emits is simple, but correctness
//! details matter for us — colons in metric names, histogram families whose
//! name already ends in `_total`, summaries without quantiles, `+Inf` bucket
//! bounds, NaN values, and escaped label values. Owning ~300 lines that are
//! table-tested against a live capture is safer than depending on a loosely
//! maintained parser crate. The rest of the codebase only sees
//! [`ScrapeText`], so this module can be swapped out.
//!
//! Leniency policy: invalid lines are skipped and reported as [`ParseIssue`]s;
//! a scrape never fails wholesale because of one bad line. Unknown comment
//! lines and OpenMetrics `# EOF` are ignored.

use std::collections::HashMap;

use super::model::{LabelSet, MetricFamily, MetricType, ParseIssue, Sample, ScrapeText};

/// A hostile or broken endpoint can make every line invalid (megabytes of
/// them within the body cap), and issue messages can embed input tokens of
/// arbitrary length — so issues are counted in full but stored bounded.
pub const MAX_PARSE_ISSUES: usize = 64;
/// Stored issue messages are truncated to this many bytes (char-boundary
/// aligned).
pub const MAX_ISSUE_MESSAGE_BYTES: usize = 120;

/// Parse a full exposition document.
pub fn parse_text(input: &str) -> ScrapeText {
    // Tolerate a UTF-8 BOM (some proxies/files prepend one).
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut out = ScrapeText::default();
    // Family name -> index into out.families.
    let mut index: HashMap<String, usize> = HashMap::new();

    for (line_no, raw_line) in input.lines().enumerate() {
        let line_no = line_no + 1;
        let line = raw_line.trim_end_matches(['\r']);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            handle_comment(rest.trim_start(), &mut out, &mut index);
            continue;
        }
        match parse_sample_line(line) {
            Ok(sample) => attach_sample(sample, &mut out, &mut index),
            Err(msg) => record_issue(&mut out, line_no, msg),
        }
    }
    out
}

/// Count every issue, but store only the first [`MAX_PARSE_ISSUES`], each
/// message truncated to [`MAX_ISSUE_MESSAGE_BYTES`].
fn record_issue(out: &mut ScrapeText, line: usize, mut message: String) {
    out.issue_count += 1;
    if out.issues.len() >= MAX_PARSE_ISSUES {
        return;
    }
    if message.len() > MAX_ISSUE_MESSAGE_BYTES {
        let mut end = MAX_ISSUE_MESSAGE_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    out.issues.push(ParseIssue { line, message });
}

/// Ensure a family exists, returning its index.
fn ensure_family(name: &str, out: &mut ScrapeText, index: &mut HashMap<String, usize>) -> usize {
    if let Some(&i) = index.get(name) {
        return i;
    }
    out.families.push(MetricFamily {
        name: name.to_string(),
        help: None,
        kind: MetricType::Untyped,
        samples: Vec::new(),
    });
    let i = out.families.len() - 1;
    index.insert(name.to_string(), i);
    i
}

fn handle_comment(rest: &str, out: &mut ScrapeText, index: &mut HashMap<String, usize>) {
    if let Some(help) = rest.strip_prefix("HELP ") {
        let mut parts = help.splitn(2, [' ', '\t']);
        if let Some(name) = parts.next().filter(|n| !n.is_empty()) {
            let text = parts.next().unwrap_or("").trim();
            let i = ensure_family(name, out, index);
            out.families[i].help = Some(unescape_help(text));
        }
    } else if let Some(ty) = rest.strip_prefix("TYPE ") {
        let mut parts = ty.split_whitespace();
        if let (Some(name), Some(kind)) = (parts.next(), parts.next()) {
            let kind = match kind {
                "counter" => MetricType::Counter,
                "gauge" => MetricType::Gauge,
                "histogram" => MetricType::Histogram,
                "summary" => MetricType::Summary,
                _ => MetricType::Untyped,
            };
            let i = ensure_family(name, out, index);
            out.families[i].kind = kind;
        }
    }
    // Anything else (plain comments, OpenMetrics "# EOF", "# UNIT") is ignored.
}

/// Attach a parsed sample to its owning family.
///
/// Histogram samples (`X_bucket`, `X_sum`, `X_count`) and summary samples
/// (`X_sum`, `X_count`, bare `X` with a `quantile` label) belong to family `X`
/// when such a family was declared. Otherwise the sample founds an untyped
/// family under its own full name.
fn attach_sample(sample: Sample, out: &mut ScrapeText, index: &mut HashMap<String, usize>) {
    // Exact-name family (covers gauges, counters, summary quantile lines).
    if let Some(&i) = index.get(sample.name.as_str()) {
        out.families[i].samples.push(sample);
        return;
    }
    // Suffixed component of a declared histogram/summary family.
    for suffix in ["_bucket", "_sum", "_count"] {
        if let Some(base) = sample.name.strip_suffix(suffix)
            && let Some(&i) = index.get(base)
        {
            let kind = out.families[i].kind;
            let histogram_part = kind == MetricType::Histogram;
            let summary_part = kind == MetricType::Summary && suffix != "_bucket";
            if histogram_part || summary_part {
                out.families[i].samples.push(sample);
                return;
            }
        }
    }
    let name = sample.name.clone();
    let i = ensure_family(&name, out, index);
    out.families[i].samples.push(sample);
}

/// Parse one sample line: `name[{labels}] value [timestamp]`.
fn parse_sample_line(line: &str) -> Result<Sample, String> {
    let line = line.trim();
    // Split off the metric name: ends at '{' or whitespace.
    let name_end = line
        .find(|c: char| c == '{' || c.is_whitespace())
        .ok_or_else(|| "missing value".to_string())?;
    let name = &line[..name_end];
    if name.is_empty() {
        return Err("empty metric name".to_string());
    }
    // The reference scraper tolerates blanks between name and '{'.
    let mut rest = line[name_end..].trim_start();

    let labels = if rest.starts_with('{') {
        let (labels, after) = parse_labels(rest)?;
        rest = after;
        labels
    } else {
        LabelSet::default()
    };

    let mut tokens = rest.split_whitespace();
    let value_tok = tokens.next().ok_or_else(|| "missing value".to_string())?;
    let value = parse_value(value_tok).ok_or_else(|| format!("unparseable value {value_tok:?}"))?;
    let timestamp_ms = match tokens.next() {
        Some(ts) => Some(
            ts.parse::<i64>()
                .map_err(|_| format!("unparseable timestamp {ts:?}"))?,
        ),
        None => None,
    };
    if tokens.next().is_some() {
        return Err("trailing garbage after timestamp".to_string());
    }

    Ok(Sample {
        name: name.to_string(),
        labels,
        value,
        timestamp_ms,
    })
}

/// Parse `{k="v",...}`, returning the labels and the remainder of the line.
/// Tolerates a trailing comma before `}` (the reference parser does too).
fn parse_labels(input: &str) -> Result<(LabelSet, &str), String> {
    debug_assert!(input.starts_with('{'));
    let mut chars = input.char_indices().peekable();
    chars.next(); // consume '{'
    let mut pairs: Vec<(String, String)> = Vec::new();

    loop {
        // Skip whitespace between entries.
        while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            Some(&(i, '}')) => {
                chars.next();
                let rest = &input[i + 1..];
                return Ok((LabelSet::new(pairs), rest));
            }
            Some(_) => {}
            None => return Err("unterminated label set".to_string()),
        }

        // Label name up to '='.
        let start = chars.peek().map(|&(i, _)| i).unwrap();
        let mut eq = None;
        for (i, c) in chars.by_ref() {
            if c == '=' {
                eq = Some(i);
                break;
            }
        }
        let eq = eq.ok_or_else(|| "label without '='".to_string())?;
        let label_name = input[start..eq].trim();
        if label_name.is_empty() {
            return Err("empty label name".to_string());
        }

        // Opening quote (blanks around '=' are tolerated, as in PromParser).
        while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            Some((_, '"')) => {}
            _ => return Err(format!("label {label_name:?}: value not quoted")),
        }
        // Escaped string body.
        let mut value = String::new();
        let mut closed = false;
        while let Some((_, c)) = chars.next() {
            match c {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => match chars.next() {
                    Some((_, 'n')) => value.push('\n'),
                    Some((_, '"')) => value.push('"'),
                    Some((_, '\\')) => value.push('\\'),
                    // Lenient: keep unknown escapes verbatim.
                    Some((_, other)) => {
                        value.push('\\');
                        value.push(other);
                    }
                    None => return Err("unterminated escape".to_string()),
                },
                other => value.push(other),
            }
        }
        if !closed {
            return Err(format!("label {label_name:?}: unterminated value"));
        }
        pairs.push((label_name.to_string(), value));

        // Separator: ',' or '}'.
        while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            Some(&(_, ',')) => {
                chars.next();
            }
            Some(&(_, '}')) => {}
            _ => return Err("expected ',' or '}' after label value".to_string()),
        }
    }
}

/// Parse a sample value. Accepts everything Go's `strconv.ParseFloat` does in
/// practice: decimal floats, scientific notation, and case-insensitive
/// `NaN` / `Inf` / `+Inf` / `-Inf` / `Infinity` spellings.
fn parse_value(tok: &str) -> Option<f64> {
    let lower = tok.to_ascii_lowercase();
    match lower.as_str() {
        "nan" | "+nan" | "-nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        _ => tok.parse::<f64>().ok(),
    }
}

/// Unescape `# HELP` text: `\\` and `\n` are the only defined escapes.
fn unescape_help(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_sample(text: &str) -> Sample {
        let scrape = parse_text(text);
        assert!(scrape.issues.is_empty(), "issues: {:?}", scrape.issues);
        let all: Vec<Sample> = scrape
            .families
            .iter()
            .flat_map(|f| f.samples.clone())
            .collect();
        assert_eq!(all.len(), 1, "expected exactly one sample");
        all[0].clone()
    }

    #[test]
    fn plain_gauge_no_labels() {
        let s = one_sample("process_open_fds 63.0");
        assert_eq!(s.name, "process_open_fds");
        assert!(s.labels.is_empty());
        assert_eq!(s.value, 63.0);
        assert_eq!(s.timestamp_ms, None);
    }

    #[test]
    fn colon_in_metric_name() {
        let s = one_sample(r#"vllm:num_requests_running{engine="0",model_name="m"} 4.0"#);
        assert_eq!(s.name, "vllm:num_requests_running");
        assert_eq!(s.labels.get("engine"), Some("0"));
        assert_eq!(s.labels.get("model_name"), Some("m"));
        assert_eq!(s.value, 4.0);
    }

    #[test]
    fn scientific_notation_and_timestamp() {
        let s = one_sample("m 2.4804184064e+010 1712345678901");
        assert_eq!(s.value, 2.4804184064e10);
        assert_eq!(s.timestamp_ms, Some(1712345678901));
    }

    #[test]
    fn special_values() {
        for (tok, check) in [
            ("NaN", f64::is_nan as fn(f64) -> bool),
            ("+Inf", |v| v == f64::INFINITY),
            ("-Inf", |v| v == f64::NEG_INFINITY),
            ("inf", |v| v == f64::INFINITY),
        ] {
            let s = one_sample(&format!("m {tok}"));
            assert!(check(s.value), "value for {tok} was {}", s.value);
        }
    }

    #[test]
    fn escaped_label_values() {
        let s = one_sample(r#"m{path="C:\\dir",msg="line\nbreak",q="say \"hi\""} 1"#);
        assert_eq!(s.labels.get("path"), Some(r"C:\dir"));
        assert_eq!(s.labels.get("msg"), Some("line\nbreak"));
        assert_eq!(s.labels.get("q"), Some(r#"say "hi""#));
    }

    #[test]
    fn empty_label_set_and_trailing_comma() {
        let s = one_sample("m{} 1");
        assert!(s.labels.is_empty());
        let s = one_sample(r#"m{a="b",} 2"#);
        assert_eq!(s.labels.get("a"), Some("b"));
    }

    #[test]
    fn whitespace_leniency_matches_reference_scraper() {
        // Blanks between name and '{', around '=', after ',', before '}'.
        let s = one_sample(r#"m {a = "1", b="2" , } 3"#);
        assert_eq!(s.labels.get("a"), Some("1"));
        assert_eq!(s.labels.get("b"), Some("2"));
        assert_eq!(s.value, 3.0);
        // Tabs as separators; negative timestamp (docs example).
        let s = one_sample("weird{problem=\"division by zero\"}\t+Inf\t-3982045");
        assert_eq!(s.value, f64::INFINITY);
        assert_eq!(s.timestamp_ms, Some(-3982045));
    }

    #[test]
    fn label_order_is_normalized() {
        let a = one_sample(r#"m{b="2",a="1"} 1"#);
        let b = one_sample(r#"m{a="1",b="2"} 1"#);
        assert_eq!(a.labels, b.labels);
    }

    #[test]
    fn histogram_family_grouping_with_total_suffix() {
        // Real vLLM quirk: a *histogram* family whose name ends in _total.
        let text = "\
# HELP vllm:iteration_tokens_total Histogram of number of tokens per engine_step.
# TYPE vllm:iteration_tokens_total histogram
vllm:iteration_tokens_total_bucket{le=\"1.0\"} 5.0
vllm:iteration_tokens_total_bucket{le=\"+Inf\"} 9.0
vllm:iteration_tokens_total_count 9.0
vllm:iteration_tokens_total_sum 42.0
";
        let scrape = parse_text(text);
        assert!(scrape.issues.is_empty(), "{:?}", scrape.issues);
        assert_eq!(scrape.families.len(), 1);
        let fam = scrape.family("vllm:iteration_tokens_total").unwrap();
        assert_eq!(fam.kind, MetricType::Histogram);
        assert_eq!(fam.samples.len(), 4);
    }

    #[test]
    fn summary_without_quantiles() {
        let text = "\
# TYPE http_request_size_bytes summary
http_request_size_bytes_count{handler=\"none\"} 110.0
http_request_size_bytes_sum{handler=\"none\"} 945.0
";
        let scrape = parse_text(text);
        let fam = scrape.family("http_request_size_bytes").unwrap();
        assert_eq!(fam.kind, MetricType::Summary);
        assert_eq!(fam.samples.len(), 2);
    }

    #[test]
    fn summary_with_quantile_lines() {
        let text = "\
# TYPE rpc_duration_seconds summary
rpc_duration_seconds{quantile=\"0.5\"} 4.0
rpc_duration_seconds{quantile=\"0.9\"} 8.0
rpc_duration_seconds_sum 100.0
rpc_duration_seconds_count 25.0
";
        let scrape = parse_text(text);
        assert_eq!(scrape.families.len(), 1);
        assert_eq!(
            scrape.family("rpc_duration_seconds").unwrap().samples.len(),
            4
        );
    }

    #[test]
    fn created_families_stay_separate() {
        let text = "\
# TYPE vllm:prompt_tokens_total counter
vllm:prompt_tokens_total 10.0
# TYPE vllm:prompt_tokens_created gauge
vllm:prompt_tokens_created 1.78e+09
";
        let scrape = parse_text(text);
        assert_eq!(scrape.families.len(), 2);
        assert_eq!(
            scrape.family("vllm:prompt_tokens_total").unwrap().kind,
            MetricType::Counter
        );
        assert_eq!(
            scrape.family("vllm:prompt_tokens_created").unwrap().kind,
            MetricType::Gauge
        );
    }

    #[test]
    fn unknown_metric_without_metadata_is_kept_untyped() {
        let scrape = parse_text("some_backend_specific_metric 7");
        let fam = scrape.family("some_backend_specific_metric").unwrap();
        assert_eq!(fam.kind, MetricType::Untyped);
        assert_eq!(fam.samples.len(), 1);
    }

    #[test]
    fn help_unescaping() {
        let scrape = parse_text("# HELP m Two\\nlines and a back\\\\slash\nm 1");
        assert_eq!(
            scrape.family("m").unwrap().help.as_deref(),
            Some("Two\nlines and a back\\slash")
        );
    }

    #[test]
    fn bad_lines_are_skipped_and_reported() {
        let text = "\
good_metric 1.0
this is not { valid
another_good 2.0
value_missing{a=\"b\"}
";
        let scrape = parse_text(text);
        assert_eq!(scrape.total_samples(), 2);
        assert_eq!(scrape.issues.len(), 2);
        assert_eq!(scrape.issues[0].line, 2);
        assert_eq!(scrape.issues[1].line, 4);
    }

    #[test]
    fn malformed_line_diagnostics_are_counted_but_storage_is_capped() {
        let text = "bad\n".repeat(100_000);
        let scrape = parse_text(&text);
        assert_eq!(scrape.issue_count, 100_000);
        assert_eq!(scrape.issues.len(), MAX_PARSE_ISSUES);
        assert!(
            scrape
                .issues
                .iter()
                .all(|issue| issue.message.len() <= MAX_ISSUE_MESSAGE_BYTES)
        );
    }

    #[test]
    fn openmetrics_eof_and_plain_comments_ignored() {
        let scrape = parse_text("# just a comment\nm 1\n# EOF\n");
        assert!(scrape.issues.is_empty());
        assert_eq!(scrape.total_samples(), 1);
    }

    #[test]
    fn crlf_line_endings() {
        let scrape = parse_text("m 1\r\nn 2\r\n");
        assert_eq!(scrape.total_samples(), 2);
    }

    #[test]
    fn utf8_bom_tolerated() {
        let scrape = parse_text("\u{feff}# HELP m docs\n# TYPE m gauge\nm 1\n");
        assert!(scrape.issues.is_empty(), "{:?}", scrape.issues);
        assert_eq!(scrape.family("m").unwrap().kind, MetricType::Gauge);
    }

    #[test]
    fn duplicate_series_are_preserved_for_downstream() {
        let scrape = parse_text("m 1\nm 2\n");
        assert_eq!(scrape.family("m").unwrap().samples.len(), 2);
    }
}

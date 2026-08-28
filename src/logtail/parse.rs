//! Substring parser for per-request log lines (std-only, no regex — same
//! rationale as the Prometheus parser in `metrics::parse`).
//!
//! DORMANT: this grammar is inherited from vllmtop and matches vLLM's
//! `RequestLogger` output, which SGLang never emits — so the requests pane
//! only ever shows guidance today. It is kept (with its tests) as the
//! working template for a future SGLang scheduler-activity ticker parsing
//! `Prefill batch, …` / `Decode batch, …` lines; the tailer plumbing in
//! `logtail::mod` is grammar-agnostic and fully live.
//!
//! Recognized shapes (after ANSI stripping; typical prefix
//! `INFO 08-24 10:23:45 [request_logger.py:63] `):
//! - `Received request <id>: params: SamplingParams(..., max_tokens=N, ...)`
//!   (INFO, needs the server's `--enable-log-requests`)
//! - `Request <id> details: prompt: '<python repr>', prompt_token_ids: ...`
//!   (DEBUG only — prompt previews exist only at `VLLM_LOGGING_LEVEL=DEBUG`)
//! - `Generated response <id>: output: ..., finish_reason: stop`
//!
//! Prompt previews are user-sensitive: they are bounded here and live only
//! in the in-memory request ring — never logged, recorded, or persisted.

use std::borrow::Cow;

/// Stored prompt previews are capped at this many bytes (char-boundary safe).
pub const MAX_PROMPT_PREVIEW_BYTES: usize = 120;
/// Request ids longer than this are truncated (defensive; real ids are short).
pub const MAX_REQUEST_ID_BYTES: usize = 64;

/// One parsed per-request event from a vLLM log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEvent {
    Received {
        id: String,
        max_tokens: Option<u64>,
    },
    Details {
        id: String,
        prompt_preview: String,
    },
    Finished {
        id: String,
        finish_reason: Option<String>,
    },
}

/// Strip ANSI CSI (`ESC [ … final`) and OSC (`ESC ] … BEL/ST`) sequences.
/// Borrows when the line has no escapes (the common case).
pub fn strip_ansi(line: &str) -> Cow<'_, str> {
    if !line.contains('\u{1b}') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            // CSI: ESC [ params… final byte in '@'..='~'.
            Some('[') => {
                chars.next();
                for f in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&f) {
                        break;
                    }
                }
            }
            // OSC: ESC ] … terminated by BEL or ESC \.
            Some(']') => {
                chars.next();
                while let Some(f) = chars.next() {
                    if f == '\u{07}' {
                        break;
                    }
                    if f == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Bare escape: drop it and continue.
            _ => {}
        }
    }
    Cow::Owned(out)
}

/// Parse one complete, lossy-UTF-8 line. `None` = not a request-logger line.
pub fn parse_line(raw: &str) -> Option<LogEvent> {
    let line = strip_ansi(raw);
    let line = line.as_ref();
    if let Some(rest) = find_after(line, "Received request ") {
        return parse_received(rest);
    }
    if let Some(rest) = find_after(line, "Generated response ") {
        return parse_finished(rest);
    }
    // DEBUG details line: "Request <id> details: prompt: …"
    if let Some(rest) = find_after(line, "Request ")
        && rest.contains(" details:")
    {
        return parse_details(rest);
    }
    None
}

/// The text after the first occurrence of `marker`.
fn find_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    line.find(marker).map(|i| &line[i + marker.len()..])
}

fn cap_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn parse_received(rest: &str) -> Option<LogEvent> {
    // "<id>: params: SamplingParams(..., max_tokens=N, ...), lora_request: …"
    let colon = rest.find(':')?;
    let id = rest[..colon].trim();
    if id.is_empty() || id.contains(' ') {
        return None;
    }
    let after = &rest[colon + 1..];
    // First "max_tokens=" in the params repr; the key appears exactly once.
    let max_tokens = find_after(after, "max_tokens=").and_then(|v| {
        let end = v.find([',', ')']).unwrap_or(v.len());
        v[..end].trim().parse::<u64>().ok()
    });
    Some(LogEvent::Received {
        id: cap_str(id, MAX_REQUEST_ID_BYTES),
        max_tokens,
    })
}

fn parse_details(rest: &str) -> Option<LogEvent> {
    // "<id> details: prompt: '<repr>', prompt_token_ids: …"
    let details = rest.find(" details:")?;
    let id = rest[..details].trim();
    if id.is_empty() || id.contains(' ') {
        return None;
    }
    let after = &rest[details..];
    let prompt = find_after(after, "prompt: ")?;
    let preview = parse_python_repr_prefix(prompt);
    Some(LogEvent::Details {
        id: cap_str(id, MAX_REQUEST_ID_BYTES),
        prompt_preview: preview,
    })
}

fn parse_finished(rest: &str) -> Option<LogEvent> {
    // "<id>: output: …, finish_reason: stop"
    let colon = rest.find(':')?;
    let id = rest[..colon].trim();
    if id.is_empty() || id.contains(' ') {
        return None;
    }
    let finish_reason = find_after(rest, "finish_reason: ").map(|v| {
        let end = v.find([',']).unwrap_or(v.len());
        v[..end].trim().trim_end_matches('.').to_string()
    });
    Some(LogEvent::Finished {
        id: cap_str(id, MAX_REQUEST_ID_BYTES),
        finish_reason,
    })
}

/// Decode the prefix of a Python string repr (`'…'` or `"…"`): honor
/// backslash escapes, map newlines/tabs to spaces, scrub control characters,
/// stop at the unescaped closing quote or end of line (a truncated line
/// keeps the prefix), and cap the result at [`MAX_PROMPT_PREVIEW_BYTES`].
fn parse_python_repr_prefix(s: &str) -> String {
    let mut chars = s.chars();
    let quote = match chars.next() {
        Some(q @ ('\'' | '"')) => q,
        // Not a quoted repr (e.g. `None`): empty preview.
        _ => return String::new(),
    };
    let mut out = String::new();
    let mut escaped = false;
    for c in chars {
        if out.len() >= MAX_PROMPT_PREVIEW_BYTES {
            break;
        }
        if escaped {
            escaped = false;
            match c {
                'n' | 't' | 'r' => out.push(' '),
                other => out.push(other),
            }
            continue;
        }
        match c {
            '\\' => escaped = true,
            c if c == quote => break,
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    // The cap check above is pre-push; one multi-byte char may overshoot.
    cap_str(&out, MAX_PROMPT_PREVIEW_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "INFO 08-24 10:23:45 [request_logger.py:63] ";

    #[test]
    fn received_line_parses_id_and_max_tokens() {
        let line = format!(
            "{PREFIX}Received request chatcmpl-8e66f5c2: params: SamplingParams(n=1, \
             temperature=0.7, stop=[], guided_decoding=GuidedDecodingParams(json=None), \
             max_tokens=1024, extra_args=None), lora_request: None."
        );
        assert_eq!(
            parse_line(&line),
            Some(LogEvent::Received {
                id: "chatcmpl-8e66f5c2".into(),
                max_tokens: Some(1024),
            })
        );
    }

    #[test]
    fn received_max_tokens_none_maps_to_option_none() {
        let line = format!(
            "{PREFIX}Received request cmpl-1: params: SamplingParams(n=1, max_tokens=None), \
             lora_request: None."
        );
        assert_eq!(
            parse_line(&line),
            Some(LogEvent::Received {
                id: "cmpl-1".into(),
                max_tokens: None,
            })
        );
    }

    #[test]
    fn received_without_max_tokens_key_still_yields_entry() {
        let line = format!(
            "{PREFIX}Received request pool-9: params: PoolingParams(), \
             lora_request: None."
        );
        assert_eq!(
            parse_line(&line),
            Some(LogEvent::Received {
                id: "pool-9".into(),
                max_tokens: None,
            })
        );
    }

    #[test]
    fn received_missing_colon_after_id_is_rejected() {
        assert_eq!(parse_line("Received request cmpl-truncated"), None);
    }

    #[test]
    fn details_line_extracts_and_unescapes_prompt_preview() {
        let line = format!(
            "{PREFIX}Request cmpl-2 details: prompt: 'An example question with \\'quotes\\' \
             and\\na newline?', prompt_token_ids: [1, 2, 3], prompt_embeds shape: None."
        );
        assert_eq!(
            parse_line(&line),
            Some(LogEvent::Details {
                id: "cmpl-2".into(),
                prompt_preview: "An example question with 'quotes' and a newline?".into(),
            })
        );
    }

    #[test]
    fn details_double_quoted_repr_supported() {
        let line = format!("{PREFIX}Request cmpl-3 details: prompt: \"it's an example\", x: 1");
        assert_eq!(
            parse_line(&line),
            Some(LogEvent::Details {
                id: "cmpl-3".into(),
                prompt_preview: "it's an example".into(),
            })
        );
    }

    #[test]
    fn prompt_preview_capped_on_char_boundary() {
        let long = "ẞ".repeat(200); // 2 bytes each: overshoots the cap
        let line = format!("Request cmpl-4 details: prompt: '{long}'");
        let Some(LogEvent::Details { prompt_preview, .. }) = parse_line(&line) else {
            panic!("expected details");
        };
        assert!(prompt_preview.len() <= MAX_PROMPT_PREVIEW_BYTES);
        assert!(prompt_preview.chars().all(|c| c == 'ẞ'));
    }

    #[test]
    fn prompt_preview_scrubs_control_characters() {
        let line = "Request cmpl-5 details: prompt: 'a\u{7}b\u{1}c'";
        let Some(LogEvent::Details { prompt_preview, .. }) = parse_line(line) else {
            panic!("expected details");
        };
        assert_eq!(prompt_preview, "a b c");
    }

    #[test]
    fn details_truncated_before_closing_quote_keeps_prefix() {
        let line = "Request cmpl-6 details: prompt: 'cut off mid";
        assert_eq!(
            parse_line(line),
            Some(LogEvent::Details {
                id: "cmpl-6".into(),
                prompt_preview: "cut off mid".into(),
            })
        );
    }

    #[test]
    fn finished_line_marks_completion_with_reason() {
        let line = format!(
            "{PREFIX}Generated response cmpl-2: output: 'It is an example answer.', \
             finish_reason: stop"
        );
        assert_eq!(
            parse_line(&line),
            Some(LogEvent::Finished {
                id: "cmpl-2".into(),
                finish_reason: Some("stop".into()),
            })
        );
    }

    #[test]
    fn ansi_colored_lines_parse_identically() {
        let plain = "Received request cmpl-7: params: SamplingParams(max_tokens=8), x";
        let colored = format!("\u{1b}[32mINFO\u{1b}[0m {plain}");
        assert_eq!(parse_line(plain), parse_line(&colored));
        assert!(parse_line(&colored).is_some());
    }

    #[test]
    fn strip_ansi_borrows_when_clean_and_removes_csi_osc() {
        assert!(matches!(strip_ansi("plain"), Cow::Borrowed(_)));
        assert_eq!(strip_ansi("\u{1b}[1;32mbold\u{1b}[0m"), "bold");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}text"), "text");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{1b}\\text"), "text");
    }

    #[test]
    fn unrelated_engine_lines_return_none() {
        for line in [
            "INFO [loggers.py:123] Engine 000: Avg prompt throughput: 102.1 tokens/s",
            "INFO [async_llm.py:456] Request cache hit rate: 55.1%", // no " details:"
            "",
        ] {
            assert_eq!(parse_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn request_id_capped_at_max_bytes() {
        let id = "x".repeat(300);
        let line = format!("Received request {id}: params: SamplingParams()");
        let Some(LogEvent::Received { id, .. }) = parse_line(&line) else {
            panic!("expected received");
        };
        assert_eq!(id.len(), MAX_REQUEST_ID_BYTES);
    }
}

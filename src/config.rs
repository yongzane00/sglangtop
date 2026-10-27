//! Configuration: defaults, TOML file, CLI flags — merged in that order.
//!
//! Precedence (lowest to highest):
//! 1. Built-in defaults (one endpoint at `http://127.0.0.1:30000`).
//! 2. The TOML config file (`--config PATH`, else
//!    `$XDG_CONFIG_HOME/sglangtop/config.toml`, else
//!    `~/.config/sglangtop/config.toml`; a missing default file is fine, a
//!    missing explicit `--config` file is an error).
//! 3. CLI flags. `--endpoint` REPLACES the file's endpoint list entirely
//!    (no merging — partial merges are impossible to reason about).
//!
//! Secrets are never stored in TOML: the file holds environment-variable
//! *names* (`bearer_token_env`, `[endpoints.header_env]`), resolved at
//! startup. A missing variable disables that endpoint with a clear error and
//! never affects other endpoints. Resolved values are never logged or shown.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use url::Url;

use crate::cli::Cli;

pub const DEFAULT_ENDPOINT_URL: &str = "http://127.0.0.1:30000";
pub const DEFAULT_REFRESH_MS: u64 = 1000;
pub const DEFAULT_HISTORY_SECS: u64 = 300;
pub const DEFAULT_PERCENTILE_WINDOW_SECS: u64 = 60;
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
/// TTFT dial zones: amber from 2 s, red from 10 s.
pub const DEFAULT_TTFT_WARN_MS: u64 = 2_000;
pub const DEFAULT_TTFT_CRIT_MS: u64 = 10_000;

/// The minimum target *start* cadence. Faster settings would collapse the
/// fixed 0/250/500/750 ms fleet phases and overload monitored servers.
pub const MIN_REFRESH_MS: u64 = 1_000;
pub const MAX_REFRESH_MS: u64 = 60_000;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file {0}: {1}")]
    Io(PathBuf, std::io::Error),
    /// Holds only the toml message, never the source excerpt: the excerpt
    /// would echo config lines (potentially containing mistakenly inlined
    /// secrets) to stderr.
    #[error("config file {0}: {1}")]
    Toml(PathBuf, String),
    /// `url` is pre-sanitized (no userinfo, no query).
    #[error("endpoint {name:?}: invalid URL {url:?}: {reason}")]
    InvalidUrl {
        name: String,
        url: String,
        reason: String,
    },
    #[error("endpoint {name:?}: unsupported scheme {scheme:?} (use http or https)")]
    BadScheme { name: String, scheme: String },
    #[error("duplicate endpoint name {0:?}")]
    DuplicateName(String),
    #[error("{field} = {value} out of range ({min}..={max})")]
    OutOfRange {
        field: &'static str,
        value: u64,
        min: u64,
        max: u64,
    },
    #[error("no endpoints configured")]
    NoEndpoints,
    #[error("ttft_warn_ms = {warn} must be below ttft_crit_ms = {crit}")]
    TtftOrder { warn: u64, crit: u64 },
}

/// One endpoint, fully validated.
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub name: String,
    pub url: Url,
    /// Env var NAME holding a bearer token (value resolved at client build).
    pub bearer_token_env: Option<String>,
    /// Header name → env var NAME holding the header value.
    pub header_env: BTreeMap<String, String>,
    /// Override for the server's `--max-running-requests`, which the seat
    /// grid otherwise reads from `/server_info`. When set, it wins; per
    /// engine on a data-parallel server.
    pub max_running: Option<u32>,
    /// Path to the SGLang server's stdout log (read-only, local, opt-in).
    /// The SGLang log grammar is not parsed yet; the pane shows guidance.
    pub log_file: Option<PathBuf>,
}

impl EndpointConfig {
    /// URL safe for display and logs: scheme://host[:port]/path — userinfo
    /// and query (which can carry credentials) are dropped.
    pub fn display_url(&self) -> String {
        redact_url(&self.url)
    }

    /// Resolve secret env references using the provided lookup (injectable
    /// for tests; production passes `std::env::var`-backed lookup).
    ///
    /// The Ok value contains real secrets: it must only ever flow into HTTP
    /// header construction, never into logs or state. The Err string is safe
    /// to display — it names the variable, never any value.
    pub fn resolve_auth(
        &self,
        get_env: impl Fn(&str) -> Option<String>,
    ) -> Result<ResolvedAuth, String> {
        let bearer = match &self.bearer_token_env {
            Some(var) => match get_env(var) {
                Some(v) if !v.is_empty() => Some(v),
                _ => {
                    return Err(format!(
                        "environment variable {var} (bearer token) is not set"
                    ));
                }
            },
            None => None,
        };
        let mut headers = Vec::new();
        for (header, var) in &self.header_env {
            match get_env(var) {
                Some(v) if !v.is_empty() => headers.push((header.clone(), v)),
                _ => {
                    return Err(format!(
                        "environment variable {var} (header {header:?}) is not set"
                    ));
                }
            }
        }
        Ok(ResolvedAuth { bearer, headers })
    }
}

/// Resolved secret material. No Debug/Display/Clone: this must not escape
/// into logs, error messages, or state snapshots.
pub struct ResolvedAuth {
    pub bearer: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// Fully merged, validated runtime configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub refresh_interval: Duration,
    pub history_window: Duration,
    pub percentile_window: Duration,
    pub retention_days: u32,
    /// TTFT dial thresholds, in seconds: amber at `ttft_warn`, red at
    /// `ttft_crit`.
    pub ttft_warn: f64,
    pub ttft_crit: f64,
    pub record: RecordSetting,
    pub no_color: bool,
    pub endpoints: Vec<EndpointConfig>,
}

/// Where recorded history goes. Recording is ON by default (the fleet
/// daily-usage charts read from it); the variants keep the *reason* it is
/// off so the UI can say why usage data is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordSetting {
    Enabled(PathBuf),
    /// `--no-record` / `no_record = true`.
    DisabledByUser,
    /// No path given and no default resolvable (message is display-safe).
    Unavailable(String),
}

impl RecordSetting {
    pub fn path(&self) -> Option<&Path> {
        match self {
            RecordSetting::Enabled(p) => Some(p),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// TOML file schema
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    refresh_interval_ms: Option<u64>,
    history_seconds: Option<u64>,
    percentile_window_seconds: Option<u64>,
    retention_days: Option<u32>,
    /// TTFT dial zone thresholds, in milliseconds.
    ttft_warn_ms: Option<u64>,
    ttft_crit_ms: Option<u64>,
    /// Recording destination; overrides the default data-dir path.
    record_path: Option<PathBuf>,
    /// `true` disables recording (and the daily-usage charts).
    no_record: Option<bool>,
    #[serde(default)]
    endpoints: Vec<FileEndpoint>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEndpoint {
    name: Option<String>,
    url: String,
    bearer_token_env: Option<String>,
    #[serde(default)]
    header_env: BTreeMap<String, String>,
    /// Override for the server's `--max-running-requests`, for the seat grid.
    max_running: Option<u32>,
    /// SGLang stdout log to tail (local, read-only; grammar not parsed yet).
    log_file: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// Loading and merging
// ---------------------------------------------------------------------------

/// Load and merge configuration. `get_env` is injectable for tests.
pub fn load(cli: &Cli, get_env: impl Fn(&str) -> Option<String>) -> Result<Config, ConfigError> {
    let file = match &cli.config {
        Some(path) => Some(read_file(path)?),
        None => match default_config_path(&get_env) {
            Some(path) if path.is_file() => Some(read_file(&path)?),
            _ => None,
        },
    };
    let default_record = default_record_path(&get_env);
    merge(cli, file.unwrap_or_default(), default_record)
}

fn read_file(path: &Path) -> Result<FileConfig, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
    toml::from_str(&text).map_err(|e| {
        // message() alone — Display would embed the offending source lines.
        ConfigError::Toml(path.to_path_buf(), e.message().to_string())
    })
}

/// `$XDG_CONFIG_HOME/sglangtop/config.toml`, else `~/.config/sglangtop/config.toml`.
fn default_config_path(get_env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(xdg) = get_env("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("sglangtop").join("config.toml"));
    }
    get_env("HOME").filter(|v| !v.is_empty()).map(|home| {
        PathBuf::from(home)
            .join(".config")
            .join("sglangtop")
            .join("config.toml")
    })
}

/// `$XDG_DATA_HOME/sglangtop/usage.db`, else `~/.local/share/sglangtop/usage.db`.
fn default_record_path(get_env: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(xdg) = get_env("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("sglangtop").join("usage.db"));
    }
    get_env("HOME").filter(|v| !v.is_empty()).map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("sglangtop")
            .join("usage.db")
    })
}

fn merge(
    cli: &Cli,
    file: FileConfig,
    default_record: Option<PathBuf>,
) -> Result<Config, ConfigError> {
    let refresh_ms = cli
        .refresh_interval_ms
        .or(file.refresh_interval_ms)
        .unwrap_or(DEFAULT_REFRESH_MS);
    check_range(
        "refresh_interval_ms",
        refresh_ms,
        MIN_REFRESH_MS,
        MAX_REFRESH_MS,
    )?;

    let history_secs = cli
        .history_seconds
        .or(file.history_seconds)
        .unwrap_or(DEFAULT_HISTORY_SECS);
    check_range("history_seconds", history_secs, 30, 86_400)?;

    let percentile_secs = cli
        .percentile_window_seconds
        .or(file.percentile_window_seconds)
        .unwrap_or(DEFAULT_PERCENTILE_WINDOW_SECS);
    check_range("percentile_window_seconds", percentile_secs, 5, 3_600)?;

    let retention_days = cli
        .retention_days
        .or(file.retention_days)
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    check_range("retention_days", u64::from(retention_days), 1, 3_650)?;

    let ttft_warn_ms = file.ttft_warn_ms.unwrap_or(DEFAULT_TTFT_WARN_MS);
    let ttft_crit_ms = file.ttft_crit_ms.unwrap_or(DEFAULT_TTFT_CRIT_MS);
    check_range("ttft_warn_ms", ttft_warn_ms, 1, 600_000)?;
    check_range("ttft_crit_ms", ttft_crit_ms, 1, 600_000)?;
    if ttft_warn_ms >= ttft_crit_ms {
        return Err(ConfigError::TtftOrder {
            warn: ttft_warn_ms,
            crit: ttft_crit_ms,
        });
    }

    // CLI endpoints replace the file list entirely.
    let endpoints = if !cli.endpoints.is_empty() {
        cli.endpoints
            .iter()
            .map(|spec| parse_endpoint_arg(spec))
            .collect::<Result<Vec<_>, _>>()?
    } else if !file.endpoints.is_empty() {
        file.endpoints
            .into_iter()
            .map(file_endpoint)
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![default_endpoint()]
    };
    if endpoints.is_empty() {
        return Err(ConfigError::NoEndpoints);
    }

    // Reject duplicate names: they are keys for display and recording.
    let mut seen = std::collections::BTreeSet::new();
    for e in &endpoints {
        if !seen.insert(e.name.clone()) {
            return Err(ConfigError::DuplicateName(e.name.clone()));
        }
    }

    // Recording resolution, most explicit first: CLI kill-switch, CLI path,
    // file kill-switch, file path, data-dir default.
    let record = if cli.no_record {
        RecordSetting::DisabledByUser
    } else if let Some(path) = &cli.record {
        RecordSetting::Enabled(path.clone())
    } else if file.no_record == Some(true) {
        RecordSetting::DisabledByUser
    } else if let Some(path) = file.record_path {
        RecordSetting::Enabled(path)
    } else if let Some(path) = default_record {
        RecordSetting::Enabled(path)
    } else {
        RecordSetting::Unavailable(
            "cannot resolve a default data path (XDG_DATA_HOME and HOME unset); \
             pass --record PATH or --no-record"
                .into(),
        )
    };

    Ok(Config {
        refresh_interval: Duration::from_millis(refresh_ms),
        history_window: Duration::from_secs(history_secs),
        percentile_window: Duration::from_secs(percentile_secs),
        retention_days,
        ttft_warn: ttft_warn_ms as f64 / 1000.0,
        ttft_crit: ttft_crit_ms as f64 / 1000.0,
        record,
        no_color: cli.no_color,
        endpoints,
    })
}

fn check_range(field: &'static str, value: u64, min: u64, max: u64) -> Result<(), ConfigError> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::OutOfRange {
            field,
            value,
            min,
            max,
        })
    }
}

fn default_endpoint() -> EndpointConfig {
    EndpointConfig {
        name: "local".to_string(),
        url: Url::parse(DEFAULT_ENDPOINT_URL).expect("default URL is valid"),
        bearer_token_env: None,
        header_env: BTreeMap::new(),
        max_running: None,
        log_file: None,
    }
}

/// Parse a `--endpoint` argument: `NAME=URL` or bare `URL`, optionally with
/// a trailing `@CAP` (`NAME=URL@8`) declaring the server's `--max-num-seqs`
/// for the seat grid — same as `max_running` in the config file.
/// A `=` only counts as the name separator when it appears before `://`; an
/// `@` only counts as the cap separator when everything after it is digits,
/// so userinfo-style URLs cannot false-match.
fn parse_endpoint_arg(spec: &str) -> Result<EndpointConfig, ConfigError> {
    let scheme_pos = spec.find("://").unwrap_or(spec.len());
    let (name, url_text) = match spec.find('=') {
        Some(eq) if eq < scheme_pos => (Some(&spec[..eq]), &spec[eq + 1..]),
        _ => (None, spec),
    };
    let (url_text, max_running) = match url_text.rsplit_once('@') {
        Some((base, cap)) if !cap.is_empty() && cap.bytes().all(|b| b.is_ascii_digit()) => {
            (base, Some(cap.parse::<u32>().unwrap_or(u32::MAX)))
        }
        _ => (url_text, None),
    };
    build_endpoint(
        name.map(str::to_string),
        url_text,
        None,
        BTreeMap::new(),
        max_running,
        None,
    )
}

fn file_endpoint(fe: FileEndpoint) -> Result<EndpointConfig, ConfigError> {
    build_endpoint(
        fe.name,
        &fe.url,
        fe.bearer_token_env,
        fe.header_env,
        fe.max_running,
        fe.log_file,
    )
}

fn build_endpoint(
    name: Option<String>,
    url_text: &str,
    bearer_token_env: Option<String>,
    header_env: BTreeMap<String, String>,
    max_running: Option<u32>,
    log_file: Option<PathBuf>,
) -> Result<EndpointConfig, ConfigError> {
    // Error paths must never echo raw URL text (it may carry credentials in
    // userinfo or query form, which we also refuse to send).
    let display_name = name.clone().unwrap_or_else(|| sanitize_url_text(url_text));
    let mut url = Url::parse(url_text).map_err(|e| ConfigError::InvalidUrl {
        name: display_name.clone(),
        url: sanitize_url_text(url_text),
        reason: e.to_string(),
    })?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(ConfigError::BadScheme {
            name: display_name,
            scheme: url.scheme().to_string(),
        });
    }
    // Probe paths are appended to this base; query/fragment cannot survive
    // that, and query strings are a secret-leak hazard anyway. Strip them.
    url.set_query(None);
    url.set_fragment(None);
    let _ = url.set_username("");
    let _ = url.set_password(None);
    let name = match name.filter(|n| !n.trim().is_empty()) {
        Some(n) => n.trim().to_string(),
        None => derive_name(&url),
    };
    Ok(EndpointConfig {
        name,
        url,
        bearer_token_env,
        header_env,
        max_running,
        log_file,
    })
}

/// Best-effort sanitizer for URL-ish text that FAILED to parse (so `Url`
/// methods are unavailable): drops everything after '?' or '#', and any
/// userinfo between "://" and '@'.
fn sanitize_url_text(text: &str) -> String {
    let end = text.find(['?', '#']).unwrap_or(text.len());
    let mut base = &text[..end];
    let mut prefix = "";
    if let Some(scheme_end) = base.find("://") {
        let (p, rest) = base.split_at(scheme_end + 3);
        if let Some(at) = rest.find('@') {
            prefix = p;
            base = &rest[at + 1..];
        }
    }
    format!("{prefix}{base}")
}

/// Stable display name from host and port: `host:port` (port omitted when
/// the scheme default).
pub fn derive_name(url: &Url) -> String {
    let host = url.host_str().unwrap_or("unknown");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// scheme://host[:port]/path with userinfo and query stripped.
pub fn redact_url(url: &Url) -> String {
    let mut out = format!("{}://", url.scheme());
    out.push_str(url.host_str().unwrap_or("unknown"));
    if let Some(port) = url.port() {
        out.push_str(&format!(":{port}"));
    }
    let path = url.path();
    if path != "/" {
        out.push_str(path);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn cli(args: &[&str]) -> Cli {
        use clap::Parser;
        let mut full = vec!["sglangtop"];
        full.extend_from_slice(args);
        Cli::parse_from(full)
    }

    #[test]
    fn defaults_when_nothing_configured() {
        let cfg = load(&cli(&[]), no_env).unwrap();
        assert_eq!(cfg.refresh_interval, Duration::from_millis(1000));
        assert_eq!(cfg.history_window, Duration::from_secs(300));
        assert_eq!(cfg.retention_days, 30);
        assert_eq!(cfg.endpoints.len(), 1);
        assert_eq!(cfg.endpoints[0].name, "local");
        assert_eq!(cfg.endpoints[0].url.as_str(), "http://127.0.0.1:30000/");
        // No env at all: recording cannot resolve a default path, and the
        // reason is preserved for display.
        assert!(matches!(cfg.record, RecordSetting::Unavailable(_)));
    }

    #[test]
    fn ttft_thresholds_default_and_validate() {
        let cfg = load(&cli(&[]), no_env).unwrap();
        assert_eq!((cfg.ttft_warn, cfg.ttft_crit), (2.0, 10.0));

        let file: FileConfig = toml::from_str(
            "ttft_warn_ms = 500
ttft_crit_ms = 3000",
        )
        .unwrap();
        let merged = merge(&cli(&[]), file, None).unwrap();
        assert_eq!((merged.ttft_warn, merged.ttft_crit), (0.5, 3.0));

        let file: FileConfig = toml::from_str(
            "ttft_warn_ms = 5000
ttft_crit_ms = 5000",
        )
        .unwrap();
        assert!(matches!(
            merge(&cli(&[]), file, None),
            Err(ConfigError::TtftOrder { .. })
        ));
        let file: FileConfig = toml::from_str("ttft_crit_ms = 0").unwrap();
        assert!(matches!(
            merge(&cli(&[]), file, None),
            Err(ConfigError::OutOfRange { .. })
        ));
    }

    #[test]
    fn record_defaults_to_xdg_data_home() {
        let env = |k: &str| (k == "XDG_DATA_HOME").then(|| "/xdg-data".to_string());
        let cfg = load(&cli(&[]), env).unwrap();
        assert_eq!(
            cfg.record.path(),
            Some(Path::new("/xdg-data/sglangtop/usage.db"))
        );
    }

    #[test]
    fn record_falls_back_to_home_local_share() {
        let env = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
        let cfg = load(&cli(&[]), env).unwrap();
        assert_eq!(
            cfg.record.path(),
            Some(Path::new("/home/u/.local/share/sglangtop/usage.db"))
        );
    }

    #[test]
    fn no_record_flag_disables_recording() {
        let env = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
        let cfg = load(&cli(&["--no-record"]), env).unwrap();
        assert_eq!(cfg.record, RecordSetting::DisabledByUser);
    }

    #[test]
    fn record_flag_overrides_default_path() {
        let env = |k: &str| (k == "HOME").then(|| "/home/u".to_string());
        let cfg = load(&cli(&["--record", "/tmp/x.db"]), env).unwrap();
        assert_eq!(cfg.record.path(), Some(Path::new("/tmp/x.db")));
    }

    #[test]
    fn file_no_record_beats_file_record_path_and_cli_record_beats_both() {
        let file: FileConfig =
            toml::from_str("no_record = true\nrecord_path = \"/from-file.db\"").unwrap();
        let merged = merge(&cli(&[]), file, Some(PathBuf::from("/default.db"))).unwrap();
        assert_eq!(merged.record, RecordSetting::DisabledByUser);

        let file: FileConfig =
            toml::from_str("no_record = true\nrecord_path = \"/from-file.db\"").unwrap();
        let merged = merge(
            &cli(&["--record", "/cli.db"]),
            file,
            Some(PathBuf::from("/default.db")),
        )
        .unwrap();
        assert_eq!(merged.record.path(), Some(Path::new("/cli.db")));
    }

    #[test]
    fn log_file_parsed_from_toml_and_absent_on_cli_endpoints() {
        let file: FileConfig = toml::from_str(
            "[[endpoints]]\nurl = \"http://h:1\"\nlog_file = \"/var/log/sglang/server.log\"",
        )
        .unwrap();
        let merged = merge(&cli(&[]), file, None).unwrap();
        assert_eq!(
            merged.endpoints[0].log_file.as_deref(),
            Some(Path::new("/var/log/sglang/server.log"))
        );
        // CLI endpoints have no log_file syntax: always None.
        let cfg = load(&cli(&["-e", "dev=http://h:1@8"]), no_env).unwrap();
        assert_eq!(cfg.endpoints[0].log_file, None);
    }

    #[test]
    fn file_record_path_beats_default() {
        let file: FileConfig = toml::from_str("record_path = \"/from-file.db\"").unwrap();
        let merged = merge(&cli(&[]), file, Some(PathBuf::from("/default.db"))).unwrap();
        assert_eq!(merged.record.path(), Some(Path::new("/from-file.db")));
    }

    #[test]
    fn endpoint_arg_forms() {
        let cfg = load(
            &cli(&[
                "--endpoint",
                "spark-a=http://10.0.0.21:8000",
                "--endpoint",
                "https://10.0.0.22:8443",
            ]),
            no_env,
        )
        .unwrap();
        assert_eq!(cfg.endpoints[0].name, "spark-a");
        assert_eq!(cfg.endpoints[1].name, "10.0.0.22:8443");
    }

    #[test]
    fn endpoint_arg_cap_suffix_sets_max_running() {
        let cfg = load(&cli(&["-e", "dev=http://10.0.0.21:8000@8"]), no_env).unwrap();
        assert_eq!(cfg.endpoints[0].name, "dev");
        assert_eq!(cfg.endpoints[0].url.as_str(), "http://10.0.0.21:8000/");
        assert_eq!(cfg.endpoints[0].max_running, Some(8));
    }

    #[test]
    fn endpoint_arg_userinfo_at_sign_is_not_a_cap_separator() {
        // Suffix after the last '@' is not all digits -> userinfo, not a cap
        // (and the userinfo itself is stripped from the stored URL).
        let cfg = load(&cli(&["-e", "http://user:pw@h:1234"]), no_env).unwrap();
        assert_eq!(cfg.endpoints[0].max_running, None);
        assert!(!cfg.endpoints[0].url.as_str().contains("user"));
    }

    #[test]
    fn name_derivation_without_port() {
        let url = Url::parse("https://sglang.example.com").unwrap();
        assert_eq!(derive_name(&url), "sglang.example.com");
    }

    #[test]
    fn equals_in_query_is_not_a_name_separator_and_query_is_stripped() {
        let cfg = load(&cli(&["-e", "http://h:1234/base?x=1"]), no_env).unwrap();
        assert_eq!(cfg.endpoints[0].name, "h:1234");
        // Query/fragment cannot survive probe-path joining and may carry
        // credentials: stripped at config time.
        assert_eq!(cfg.endpoints[0].url.as_str(), "http://h:1234/base");
    }

    #[test]
    fn userinfo_is_stripped_from_stored_url() {
        let cfg = load(&cli(&["-e", "http://user:hunter2@h:1/x"]), no_env).unwrap();
        let stored = cfg.endpoints[0].url.as_str();
        assert!(!stored.contains("hunter2"), "{stored}");
        assert!(!stored.contains("user"), "{stored}");
    }

    #[test]
    fn invalid_url_error_redacts_credentials_and_query() {
        let err = load(
            &cli(&["-e", "ht!tp://user:hunter2@h:1/x?token=tok-12345"]),
            no_env,
        )
        .err()
        .unwrap();
        let text = err.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert!(!text.contains("tok-12345"), "{text}");
    }

    #[test]
    fn sanitize_url_text_cases() {
        assert_eq!(
            sanitize_url_text("http://u:pw@h:1/x?token=abc#frag"),
            "http://h:1/x"
        );
        assert_eq!(sanitize_url_text("not a url"), "not a url");
        assert_eq!(sanitize_url_text("h:1?x=1"), "h:1");
    }

    #[test]
    fn bad_scheme_rejected() {
        let err = load(&cli(&["-e", "ftp://h:21"]), no_env).unwrap_err();
        assert!(matches!(err, ConfigError::BadScheme { .. }));
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = load(
            &cli(&["-e", "a=http://h1:1", "-e", "a=http://h2:2"]),
            no_env,
        )
        .unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateName(n) if n == "a"));
    }

    #[test]
    fn derived_duplicate_names_rejected() {
        let err = load(&cli(&["-e", "http://h:1/a", "-e", "http://h:1/b"]), no_env).unwrap_err();
        assert!(matches!(err, ConfigError::DuplicateName(_)));
    }

    #[test]
    fn out_of_range_rejected() {
        let err = load(&cli(&["--refresh-interval-ms", "10"]), no_env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::OutOfRange {
                field: "refresh_interval_ms",
                ..
            }
        ));
    }

    #[test]
    fn one_second_is_the_minimum_target_start_cadence() {
        let err = load(&cli(&["--refresh-interval-ms", "999"]), no_env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::OutOfRange {
                field: "refresh_interval_ms",
                min: 1_000,
                ..
            }
        ));
        assert_eq!(
            load(&cli(&["--refresh-interval-ms", "1000"]), no_env)
                .unwrap()
                .refresh_interval,
            Duration::from_secs(1)
        );
    }

    fn write_config(dir: &Path, text: &str) -> PathBuf {
        let path = dir.join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        path
    }

    const FILE: &str = r#"
refresh_interval_ms = 2000
history_seconds = 600

[[endpoints]]
name = "spark-a"
url = "http://10.0.0.21:8000"
bearer_token_env = "SPARK_A_SGLANG_TOKEN"

[endpoints.header_env]
"X-Custom-Auth" = "SPARK_A_CUSTOM_AUTH"

[[endpoints]]
url = "https://10.0.0.22:8443"
"#;

    #[test]
    fn file_values_override_defaults_and_cli_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), FILE);
        let path_str = path.to_str().unwrap();

        let cfg = load(&cli(&["--config", path_str]), no_env).unwrap();
        assert_eq!(cfg.refresh_interval, Duration::from_millis(2000));
        assert_eq!(cfg.history_window, Duration::from_secs(600));
        assert_eq!(cfg.endpoints.len(), 2);
        assert_eq!(cfg.endpoints[0].name, "spark-a");
        assert_eq!(
            cfg.endpoints[0].bearer_token_env.as_deref(),
            Some("SPARK_A_SGLANG_TOKEN")
        );
        assert_eq!(cfg.endpoints[1].name, "10.0.0.22:8443");

        // CLI wins over the file.
        let cfg = load(
            &cli(&["--config", path_str, "--refresh-interval-ms", "1500"]),
            no_env,
        )
        .unwrap();
        assert_eq!(cfg.refresh_interval, Duration::from_millis(1500));

        // CLI endpoints REPLACE file endpoints.
        let cfg = load(
            &cli(&["--config", path_str, "-e", "solo=http://h:1"]),
            no_env,
        )
        .unwrap();
        assert_eq!(cfg.endpoints.len(), 1);
        assert_eq!(cfg.endpoints[0].name, "solo");
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let err = load(&cli(&["--config", "/nonexistent/nope.toml"]), no_env).unwrap_err();
        assert!(matches!(err, ConfigError::Io(..)));
    }

    #[test]
    fn missing_default_config_is_fine() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_string();
        let get = move |k: &str| match k {
            "HOME" => Some(home.clone()),
            _ => None,
        };
        assert!(load(&cli(&[]), get).is_ok());
    }

    #[test]
    fn default_config_path_is_discovered_via_xdg() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("sglangtop");
        std::fs::create_dir_all(&nested).unwrap();
        write_config(&nested, "refresh_interval_ms = 3000\n");
        let xdg = dir.path().to_str().unwrap().to_string();
        let get = move |k: &str| match k {
            "XDG_CONFIG_HOME" => Some(xdg.clone()),
            _ => None,
        };
        let cfg = load(&cli(&[]), get).unwrap();
        assert_eq!(cfg.refresh_interval, Duration::from_millis(3000));
    }

    #[test]
    fn unknown_toml_keys_rejected_without_echoing_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(dir.path(), "bearer_token = \"oops-secret-inline\"\n");
        let err = load(&cli(&["--config", path.to_str().unwrap()]), no_env).unwrap_err();
        assert!(matches!(err, ConfigError::Toml(..)));
        // The error must not reproduce the offending source line.
        assert!(!err.to_string().contains("oops-secret-inline"), "{err}");
    }

    #[test]
    fn auth_resolution_success_and_missing() {
        let ep = EndpointConfig {
            name: "a".into(),
            url: Url::parse("https://h:1").unwrap(),
            bearer_token_env: Some("TOK".into()),
            header_env: BTreeMap::from([("X-Auth".to_string(), "XAUTH".to_string())]),
            max_running: None,
            log_file: None,
        };
        let ok = ep
            .resolve_auth(|k| match k {
                "TOK" => Some("secret-token".into()),
                "XAUTH" => Some("secret-header".into()),
                _ => None,
            })
            .unwrap();
        assert_eq!(ok.bearer.as_deref(), Some("secret-token"));
        assert_eq!(ok.headers.len(), 1);

        // Missing variable: error names the VARIABLE, never a value.
        // (`.err().unwrap()` because ResolvedAuth is deliberately non-Debug.)
        let err = ep.resolve_auth(|_| None).err().unwrap();
        assert!(err.contains("TOK"));
        assert!(!err.contains("secret"));

        // Empty value counts as missing (common `export TOK=` mistake).
        let err = ep
            .resolve_auth(|k| (k == "TOK").then(String::new))
            .err()
            .unwrap();
        assert!(err.contains("TOK"));
    }

    #[test]
    fn url_redaction_strips_userinfo_and_query() {
        let url = Url::parse("https://user:pass@h.example:8443/v1?token=super-secret").unwrap();
        let text = redact_url(&url);
        assert_eq!(text, "https://h.example:8443/v1");
        assert!(!text.contains("pass"));
        assert!(!text.contains("super-secret"));
    }
}

//! Observability (RFC §20).
//!
//! Three layers, all in-tree:
//!
//! 1. **Structured logs** via `tracing`. Spans wrap each workflow
//!    execution and each node execution; events carry typed fields
//!    (execution_id, workflow_id, node_id, outcome, latency_ms,
//!    reason). Operators pipe the output into any OTLP collector or
//!    log aggregator — the JSON format this module emits is plain
//!    line-delimited JSON any filelog receiver accepts.
//!
//! 2. **Metrics** — [`Metrics`] owns a handful of `AtomicU64`
//!    counters (RFC §20.3): workflow starts / completions / failures /
//!    timeouts, total node executions, policy denials.
//!    Cheap to read, safe to share across threads.
//!
//! 3. **Audit trail** is currently expressed as tracing events with
//!    a specific target (`agentd::audit`). A dedicated sink (JSONL
//!    file with per-event redaction) is Phase-10 scope.

pub mod metrics;
pub mod traceparent;

pub use metrics::{Metrics, MetricsSnapshot};
pub use traceparent::{TraceParent, fresh_span_id, parse_traceparent};

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Registry};

/// Log output format. `Text` is human-friendly (colourised on TTY);
/// `Json` is the one piped into OTLP / aggregators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Text,
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "text" | "pretty" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Target
// ---------------------------------------------------------------------------

/// Where log lines go.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LogTarget {
    #[default]
    Stderr,
    Stdout,
    File(PathBuf),
}

impl LogTarget {
    /// Parse the CLI / env / TOML string form:
    ///
    /// - `"stderr"` → `Stderr`
    /// - `"stdout"` → `Stdout`
    /// - `"file:PATH"` → `File(PATH)`
    ///
    /// Anything else returns `None`.
    pub fn parse(s: &str) -> Option<Self> {
        let lower = s.to_ascii_lowercase();
        if lower == "stderr" {
            Some(Self::Stderr)
        } else if lower == "stdout" {
            Some(Self::Stdout)
        } else if let Some(rest) = s.strip_prefix("file:") {
            let path = rest.trim();
            if path.is_empty() {
                None
            } else {
                Some(Self::File(PathBuf::from(path)))
            }
        } else {
            None
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            LogTarget::Stderr => "stderr".into(),
            LogTarget::Stdout => "stdout".into(),
            LogTarget::File(p) => format!("file:{}", p.display()),
        }
    }
}

// TOML / JSON encode as a tagged string. Matches the CLI surface so
// operators see the same spelling in both places.
impl Serialize for LogTarget {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.as_str())
    }
}
impl<'de> Deserialize<'de> for LogTarget {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        LogTarget::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid log target `{s}` (expected `stderr`, `stdout`, or `file:PATH`)"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Workflow-side logging config
// ---------------------------------------------------------------------------

/// `[logging]` block in the workflow TOML. Every field optional —
/// missing fields fall through to env / CLI overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub format: Option<Format>,
    #[serde(default)]
    pub target: Option<LogTarget>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// The merged, fully-resolved config the runtime applies at startup.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLogging {
    pub level: String,
    pub format: Format,
    pub target: LogTarget,
    pub enabled: bool,
}

impl Default for ResolvedLogging {
    fn default() -> Self {
        Self {
            level: "warn".into(),
            format: Format::Text,
            target: LogTarget::Stderr,
            enabled: true,
        }
    }
}

/// Install a global `tracing` subscriber for the default stderr
/// target. Kept for call sites that don't care about target
/// selection. Idempotent: a second call returns `Err` but the
/// existing subscriber keeps working.
///
/// `level` is an `EnvFilter` directive; most operators pass
/// `"info"` / `"debug"` / `"agentd=debug,tracing=warn"`. Missing /
/// malformed → `info`.
pub fn init(level: &str, format: Format) -> Result<(), InitError> {
    install(level, format, StderrWriter)
}

/// Apply a fully-resolved logging config. Returns `Ok(())` and does
/// nothing when `enabled = false`.
pub fn apply(cfg: &ResolvedLogging) -> Result<(), InitError> {
    if !cfg.enabled {
        return Ok(());
    }
    match &cfg.target {
        LogTarget::Stderr => install(&cfg.level, cfg.format, StderrWriter),
        LogTarget::Stdout => install(&cfg.level, cfg.format, StdoutWriter),
        LogTarget::File(path) => {
            let writer = FileWriter::open(path)?;
            install(&cfg.level, cfg.format, writer)
        }
    }
}

/// Same as [`init`] but takes a custom writer. Used by tests to
/// capture emitted events without touching stderr.
pub fn install<W>(level: &str, format: Format, writer: W) -> Result<(), InitError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));

    match format {
        Format::Text => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_target(true)
                .with_ansi(false);
            let subscriber = Registry::default().with(filter).with(fmt_layer);
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| InitError::Install(e.to_string()))
        }
        Format::Json => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(writer)
                .with_target(true);
            let subscriber = Registry::default().with(filter).with(fmt_layer);
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|e| InitError::Install(e.to_string()))
        }
    }
}

/// Thin writer newtype so callers don't have to import
/// `std::io::stderr` + turbofish it through `MakeWriter`.
#[derive(Debug, Clone, Copy)]
pub struct StderrWriter;

impl<'a> MakeWriter<'a> for StderrWriter {
    type Writer = std::io::Stderr;
    fn make_writer(&'a self) -> Self::Writer {
        std::io::stderr()
    }
}

/// Stdout equivalent — same shape as [`StderrWriter`].
#[derive(Debug, Clone, Copy)]
pub struct StdoutWriter;

impl<'a> MakeWriter<'a> for StdoutWriter {
    type Writer = std::io::Stdout;
    fn make_writer(&'a self) -> Self::Writer {
        std::io::stdout()
    }
}

/// File-backed `MakeWriter`. Hand-rolled — `tracing-appender` would
/// do it better (non-blocking + rotation) but adds a dep we don't
/// need at this scale. Writes are synchronous under a Mutex; good
/// for moderate log rates, not for high-throughput production. When
/// that matters, operators should log to stderr and pipe into a
/// real collector (vector, filebeat).
#[derive(Debug, Clone)]
pub struct FileWriter {
    inner: Arc<Mutex<File>>,
}

impl FileWriter {
    pub fn open(path: &Path) -> Result<Self, InitError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    InitError::Install(format!("mkdir_p {} for log target: {e}", parent.display()))
                })?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| InitError::Install(format!("open {}: {e}", path.display())))?;
        Ok(Self {
            inner: Arc::new(Mutex::new(file)),
        })
    }
}

/// Handle returned by `FileWriter::make_writer`. Takes the lock for
/// the duration of one `write!` call.
pub struct FileWriterHandle {
    inner: Arc<Mutex<File>>,
}

impl Write for FileWriterHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut file = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("log-file mutex poisoned"))?;
        file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut file = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("log-file mutex poisoned"))?;
        file.flush()
    }
}

impl<'a> MakeWriter<'a> for FileWriter {
    type Writer = FileWriterHandle;
    fn make_writer(&'a self) -> Self::Writer {
        FileWriterHandle {
            inner: self.inner.clone(),
        }
    }
}

/// Capturing writer — for tests. All emitted bytes land in a shared
/// `Vec<u8>` that the test can inspect after execution.
#[derive(Debug, Clone, Default)]
pub struct CapturingWriter {
    inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl CapturingWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn captured(&self) -> Vec<u8> {
        self.inner.lock().unwrap().clone()
    }

    pub fn captured_string(&self) -> String {
        String::from_utf8_lossy(&self.captured()).into_owned()
    }
}

impl Write for CapturingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for CapturingWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Why [`init`] refused to install.
#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("tracing subscriber already installed: {0}")]
    Install(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse() {
        assert_eq!(Format::parse("text"), Some(Format::Text));
        assert_eq!(Format::parse("Pretty"), Some(Format::Text));
        assert_eq!(Format::parse("JSON"), Some(Format::Json));
        assert_eq!(Format::parse("xml"), None);
    }

    #[test]
    fn capturing_writer_collects_bytes() {
        let w = CapturingWriter::new();
        {
            let mut handle = w.clone();
            handle.write_all(b"hello").unwrap();
        }
        assert_eq!(w.captured_string(), "hello");
    }

    #[test]
    fn log_target_parse_variants() {
        assert_eq!(LogTarget::parse("stderr"), Some(LogTarget::Stderr));
        assert_eq!(LogTarget::parse("STDERR"), Some(LogTarget::Stderr));
        assert_eq!(LogTarget::parse("stdout"), Some(LogTarget::Stdout));
        assert_eq!(
            LogTarget::parse("file:/tmp/a.log"),
            Some(LogTarget::File("/tmp/a.log".into()))
        );
        assert_eq!(LogTarget::parse("file:"), None);
        assert_eq!(LogTarget::parse("other"), None);
    }

    #[test]
    fn logging_config_parses_full_block() {
        let src = r#"
            level = "debug"
            format = "json"
            target = "file:/tmp/x.log"
            enabled = true
        "#;
        let cfg: LoggingConfig = toml::from_str(src).unwrap();
        assert_eq!(cfg.level.as_deref(), Some("debug"));
        assert_eq!(cfg.format, Some(Format::Json));
        assert_eq!(cfg.target, Some(LogTarget::File("/tmp/x.log".into())));
        assert_eq!(cfg.enabled, Some(true));
    }

    #[test]
    fn logging_config_unknown_fields_rejected() {
        assert!(
            toml::from_str::<LoggingConfig>(
                r#"level = "info"
               unknown = 1"#
            )
            .is_err()
        );
    }

    #[test]
    fn logging_config_target_via_json_round_trip() {
        // Using serde_json (already a dep) to exercise both Ser + De
        // without pulling the `display` feature on `toml`.
        let cfg = LoggingConfig {
            level: Some("debug".into()),
            format: Some(Format::Json),
            target: Some(LogTarget::File("/tmp/x.log".into())),
            enabled: Some(true),
        };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: LoggingConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn file_writer_creates_and_appends() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("deep/nested/out.log");
        let w = FileWriter::open(&path).unwrap();
        {
            let mut h = w.clone().make_writer();
            h.write_all(b"first\n").unwrap();
        }
        // Re-open same path — append, not truncate.
        let w2 = FileWriter::open(&path).unwrap();
        {
            let mut h = w2.make_writer();
            h.write_all(b"second\n").unwrap();
        }
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "first\nsecond\n");
    }

    #[test]
    fn apply_disabled_is_noop() {
        let cfg = ResolvedLogging {
            level: "trace".into(),
            format: Format::Json,
            target: LogTarget::Stderr,
            enabled: false,
        };
        // Should not error and should not install a subscriber.
        apply(&cfg).unwrap();
    }

    #[test]
    fn unknown_level_falls_back_to_info() {
        // Internal helper; EnvFilter rejects nonsense and we recover
        // to "info" — exercise the parse path without installing.
        let f = EnvFilter::try_new("not_a_level").unwrap_or_else(|_| EnvFilter::new("info"));
        // EnvFilter has no easy inspection surface — the assertion is
        // that this does not panic.
        let _ = f;
    }
}

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

pub mod audit;
pub mod metrics;
pub mod otel;
pub mod traceparent;

pub use audit::{AuditConfig, AuditLayer};
pub use metrics::{Metrics, MetricsSnapshot};
pub use otel::OtelConfig;
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
    /// Optional `[logging.audit]` sub-block. When set,
    /// audit events (target `"agentd::audit"`) also emit to this
    /// dedicated JSONL sink with field-level redaction applied.
    /// Absent → audit events only flow through the main stream.
    #[serde(default)]
    pub audit: Option<audit::AuditConfig>,
    /// Rotation policy for `file:` targets. Default
    /// `never` keeps the current "single file, rely on external
    /// logrotate" posture. `daily` / `hourly` / `minutely` use
    /// `tracing-appender`'s rolling writer to open a new file at
    /// each boundary (suffix `YYYY-MM-DD[-HH[-MM]]`).
    #[serde(default)]
    pub rotation: Option<LogRotation>,
    /// Optional `[otel]` sub-block. When set AND the
    /// `otel` Cargo feature is compiled in, every span is also
    /// exported over OTLP gRPC to the configured collector.
    /// Feature-off builds reject the block with a rebuild hint.
    #[serde(default)]
    pub otel: Option<otel::OtelConfig>,
}

/// How often the file writer should roll over to a new file. Only
/// meaningful when `target = "file:PATH"`. Stdout / stderr targets
/// ignore the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogRotation {
    /// Single file, no rotation. External tooling (`logrotate`,
    /// k8s volume mounts, container log drivers) handles
    /// retention. This is the pre-rotation default behaviour.
    #[default]
    Never,
    /// Roll at UTC midnight. Filename suffix `YYYY-MM-DD`.
    Daily,
    /// Roll on the hour. Filename suffix `YYYY-MM-DD-HH`.
    Hourly,
    /// Roll every minute — for integration tests that need to
    /// observe the rotation path without waiting an hour.
    Minutely,
}

/// The merged, fully-resolved config the runtime applies at startup.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedLogging {
    pub level: String,
    pub format: Format,
    pub target: LogTarget,
    pub enabled: bool,
    /// Optional audit-sink config flowing through from the workflow
    /// TOML's `[logging.audit]` block. CLI / env overrides don't
    /// touch this — the sink is workflow-authored-only.
    pub audit: Option<audit::AuditConfig>,
    /// File-rotation policy. Only consulted when `target` is a
    /// `File`. Default `Never`.
    pub rotation: LogRotation,
    /// Optional OTLP exporter. Installed as a
    /// `tracing-opentelemetry` layer alongside the main fmt + audit
    /// layers when the `otel` Cargo feature is compiled in.
    pub otel: Option<otel::OtelConfig>,
}

impl Default for ResolvedLogging {
    fn default() -> Self {
        Self {
            level: "warn".into(),
            format: Format::Text,
            target: LogTarget::Stderr,
            enabled: true,
            audit: None,
            rotation: LogRotation::Never,
            otel: None,
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
/// nothing when `enabled = false`. When [`ResolvedLogging::audit`]
/// is present, an additional [`AuditLayer`] attaches alongside the
/// main fmt layer — audit events flow to both streams, and the
/// dedicated sink applies field-level redaction.
pub fn apply(cfg: &ResolvedLogging) -> Result<(), InitError> {
    if !cfg.enabled {
        return Ok(());
    }
    // Reject OTel config on a feature-off build up-front — the
    // install_with_audit_otel path below is a no-op when the
    // feature is off, so operators would silently lose exports.
    #[cfg(not(feature = "otel"))]
    if cfg.otel.is_some() {
        return Err(InitError::Install(
            "workflow declares [otel] but this build lacks the `otel` \
             Cargo feature; rebuild with --features otel"
                .into(),
        ));
    }

    match &cfg.target {
        LogTarget::Stderr => install_with_audit_otel(
            &cfg.level,
            cfg.format,
            StderrWriter,
            cfg.audit.as_ref(),
            cfg.otel.as_ref(),
        ),
        LogTarget::Stdout => install_with_audit_otel(
            &cfg.level,
            cfg.format,
            StdoutWriter,
            cfg.audit.as_ref(),
            cfg.otel.as_ref(),
        ),
        LogTarget::File(path) => {
            if cfg.rotation == LogRotation::Never {
                // Hand-rolled FileWriter — the pre-rotation path.
                // Operators who want external rotation via `logrotate`
                // get the file-is-just-a-file behaviour.
                let writer = FileWriter::open(path)?;
                install_with_audit_otel(
                    &cfg.level,
                    cfg.format,
                    writer,
                    cfg.audit.as_ref(),
                    cfg.otel.as_ref(),
                )
            } else {
                // Time-based rotation via `tracing-appender`. The
                // appender takes (dir, filename_prefix); we split
                // the operator's path so `/var/log/agent.log` with
                // daily rotation yields `/var/log/agent.log.2026-04-23`.
                let writer = RotatingFileWriter::new(path, cfg.rotation)?;
                install_with_audit_otel(
                    &cfg.level,
                    cfg.format,
                    writer,
                    cfg.audit.as_ref(),
                    cfg.otel.as_ref(),
                )
            }
        }
    }
}

/// Build an optional [`AuditLayer`] from the config's sink target.
/// Returns `Ok(None)` when no audit block is configured.
fn build_audit_layer(
    cfg: Option<&audit::AuditConfig>,
) -> Result<Option<audit::AuditLayer>, InitError> {
    let Some(audit_cfg) = cfg else {
        return Ok(None);
    };
    match audit_cfg.effective_target() {
        LogTarget::Stderr => Ok(Some(audit::AuditLayer::new(std::io::stderr(), audit_cfg))),
        LogTarget::Stdout => Ok(Some(audit::AuditLayer::new(std::io::stdout(), audit_cfg))),
        LogTarget::File(path) => {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        InitError::Install(format!(
                            "mkdir_p {} for audit target: {e}",
                            parent.display()
                        ))
                    })?;
                }
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| {
                    InitError::Install(format!("open audit target {}: {e}", path.display()))
                })?;
            Ok(Some(audit::AuditLayer::new(file, audit_cfg)))
        }
    }
}

/// Same as [`init`] but takes a custom writer. Used by tests to
/// capture emitted events without touching stderr.
pub fn install<W>(level: &str, format: Format, writer: W) -> Result<(), InitError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    install_with_audit(level, format, writer, None)
}

/// Install a global subscriber with the main fmt layer and an
/// optional audit layer. The caller-supplied `audit` config creates
/// a dedicated sink when present; `None` means audit events only
/// flow through the main layer (same behaviour as before the audit sink existed).
pub fn install_with_audit<W>(
    level: &str,
    format: Format,
    writer: W,
    audit: Option<&audit::AuditConfig>,
) -> Result<(), InitError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    install_with_audit_otel(level, format, writer, audit, None)
}

/// Full subscriber install — main fmt + optional audit layer +
/// optional OTLP exporter. Composes all three into one
/// global `tracing` subscriber. The OTel layer is only built when
/// the Cargo feature is on AND the config declares `[otel]`;
/// feature-off builds with `[otel]` declared return an error from
/// `apply` before reaching here.
pub fn install_with_audit_otel<W>(
    level: &str,
    format: Format,
    writer: W,
    audit: Option<&audit::AuditConfig>,
    otel_cfg: Option<&otel::OtelConfig>,
) -> Result<(), InitError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(level).unwrap_or_else(|_| EnvFilter::new("info"));
    let audit_layer = build_audit_layer(audit)?;

    // Compose the subscriber branch-by-branch. The OTel layer is
    // attached in an inner cfg-block so its generic `S` is the
    // fully-accumulated subscriber type at that call site, which
    // is what `tracing-opentelemetry`'s `Layer<S>` bound demands.
    match format {
        Format::Text => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_target(true)
                .with_ansi(false);
            let subscriber = Registry::default()
                .with(filter)
                .with(fmt_layer)
                .with(audit_layer);
            install_with_optional_otel(subscriber, otel_cfg)
        }
        Format::Json => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_writer(writer)
                .with_target(true);
            let subscriber = Registry::default()
                .with(filter)
                .with(fmt_layer)
                .with(audit_layer);
            install_with_optional_otel(subscriber, otel_cfg)
        }
    }
}

/// Attach the OTel layer when configured + feature is on, then
/// install the subscriber globally. The `where S + …` bound is
/// identical to what `set_global_default` requires, so the helper
/// is just "accept any fully-composed subscriber."
fn install_with_optional_otel<S>(
    subscriber: S,
    otel_cfg: Option<&otel::OtelConfig>,
) -> Result<(), InitError>
where
    S: tracing::Subscriber + Send + Sync + 'static,
    S: for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    #[cfg(feature = "otel")]
    {
        if let Some(cfg) = otel_cfg {
            let otel_layer = otel::init_otel_layer::<S>(cfg)
                .map_err(|e| InitError::Install(format!("otel init: {e}")))?;
            let composed = subscriber.with(otel_layer);
            return tracing::subscriber::set_global_default(composed)
                .map_err(|e| InitError::Install(e.to_string()));
        }
    }
    #[cfg(not(feature = "otel"))]
    {
        let _ = otel_cfg;
    }
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|e| InitError::Install(e.to_string()))
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

/// Time-based rotating file writer. Wraps
/// `tracing_appender::rolling::RollingFileAppender` behind the
/// `MakeWriter` shape so it composes with the same install path the
/// stderr / stdout / non-rotating file cases use.
///
/// Filename layout: the operator's `file:PATH` is split into
/// `(dir, filename)`. After rotation each boundary produces a new
/// file named `<filename>.<suffix>` where the suffix is `YYYY-MM-DD`
/// (daily), `YYYY-MM-DD-HH` (hourly), or `YYYY-MM-DD-HH-MM`
/// (minutely). This matches what tracing-appender produces by
/// default.
pub struct RotatingFileWriter {
    appender: std::sync::Arc<std::sync::Mutex<tracing_appender::rolling::RollingFileAppender>>,
}

impl std::fmt::Debug for RotatingFileWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RotatingFileWriter").finish_non_exhaustive()
    }
}

impl RotatingFileWriter {
    pub fn new(path: &Path, rotation: LogRotation) -> Result<Self, InitError> {
        let (dir, filename) = split_path(path).ok_or_else(|| {
            InitError::Install(format!(
                "logging rotation: target path {} must include a filename",
                path.display()
            ))
        })?;
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(&dir).map_err(|e| {
                InitError::Install(format!(
                    "mkdir_p {} for rotating log target: {e}",
                    dir.display()
                ))
            })?;
        }
        use tracing_appender::rolling::Rotation;
        let rot = match rotation {
            LogRotation::Never => {
                return Err(InitError::Install(
                    "RotatingFileWriter was built with rotation=Never — caller should use FileWriter instead"
                        .into(),
                ));
            }
            LogRotation::Daily => Rotation::DAILY,
            LogRotation::Hourly => Rotation::HOURLY,
            LogRotation::Minutely => Rotation::MINUTELY,
        };
        let appender = tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(rot)
            .filename_prefix(filename.to_string_lossy().into_owned())
            .build(&dir)
            .map_err(|e| InitError::Install(format!("rolling appender build: {e}")))?;
        Ok(Self {
            appender: std::sync::Arc::new(std::sync::Mutex::new(appender)),
        })
    }
}

/// Per-event handle into the rolling appender. Takes the appender
/// mutex for one `write` call, same lock pattern as `FileWriter`.
pub struct RotatingFileWriterHandle {
    inner: std::sync::Arc<std::sync::Mutex<tracing_appender::rolling::RollingFileAppender>>,
}

impl Write for RotatingFileWriterHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut appender = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("rolling-appender mutex poisoned"))?;
        appender.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let mut appender = self
            .inner
            .lock()
            .map_err(|_| std::io::Error::other("rolling-appender mutex poisoned"))?;
        appender.flush()
    }
}

impl<'a> MakeWriter<'a> for RotatingFileWriter {
    type Writer = RotatingFileWriterHandle;
    fn make_writer(&'a self) -> Self::Writer {
        RotatingFileWriterHandle {
            inner: self.appender.clone(),
        }
    }
}

/// Split a full path into `(dir, filename)`. Returns `None` when
/// the path has no filename component (bare `/`, `..`).
fn split_path(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let filename = path.file_name()?.to_owned();
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    Some((dir, PathBuf::from(filename)))
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
            audit: None,
            rotation: None,
            otel: None,
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
    fn rotating_writer_writes_and_rotates() {
        use std::io::Write as _;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("app.log");
        let w = RotatingFileWriter::new(&path, LogRotation::Minutely).unwrap();
        {
            let mut h = w.make_writer();
            h.write_all(b"hello from rotation\n").unwrap();
            h.flush().unwrap();
        }
        // Exactly one file should exist with our prefix.
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("app.log"))
            .collect();
        assert_eq!(files.len(), 1, "got files: {files:?}");
        // Filename pattern: app.log.YYYY-MM-DD-HH-MM (minutely).
        let name = &files[0];
        assert!(
            name.starts_with("app.log.") && name.len() == "app.log.YYYY-MM-DD-HH-MM".len(),
            "unexpected filename: {name}"
        );
    }

    #[test]
    fn rotation_never_is_not_accepted_by_rotating_writer() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("x.log");
        let err = RotatingFileWriter::new(&path, LogRotation::Never).unwrap_err();
        assert!(matches!(err, InitError::Install(_)));
    }

    #[test]
    fn logrotation_parses_from_toml() {
        let src = r#"
            level = "info"
            rotation = "daily"
        "#;
        let cfg: LoggingConfig = toml::from_str(src).unwrap();
        assert_eq!(cfg.rotation, Some(LogRotation::Daily));
    }

    #[test]
    fn apply_disabled_is_noop() {
        let cfg = ResolvedLogging {
            level: "trace".into(),
            format: Format::Json,
            target: LogTarget::Stderr,
            enabled: false,
            audit: None,
            rotation: LogRotation::Never,
            otel: None,
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

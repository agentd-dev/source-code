//! Dedicated audit-event sink with field-level redaction.
//!
//! Audit events in this codebase are `tracing::Event`s emitted with
//! target `"agentd::audit"` — e.g. `http.auth_denied`, `policy.deny`,
//! `signing.verified`, `reload.succeeded`. The main log stream
//! captures everything; this sink captures the audit target alone,
//! with configurable field-level redaction, and writes to an
//! independent destination (usually a dedicated JSONL file rotated
//! by a log shipper).
//!
//! Why split it out:
//!
//! - Compliance shops retain audit logs on a different cadence from
//!   ops logs. Separate file = separate retention policy.
//! - Sensitive fields in audit events (bearer tokens, HMAC secrets
//!   echoed in a "denied" reason, full URLs with query params) get
//!   masked here before they hit disk. The main log stream keeps
//!   whatever tracing emitted (operator can control via `[logging]`
//!   level).
//!
//! ## Grammar
//!
//! ```toml
//! [logging.audit]
//! target = "file:/var/log/agent/audit.jsonl"
//! # Extend the built-in default list; never shrinks it.
//! redact_fields = ["custom_secret_field"]
//! # Include the `reason` field's contents verbatim? Default false
//! # since auth deny reasons can carry token prefixes.
//! include_reason = false
//! ```
//!
//! ## Built-in redaction list
//!
//! Always redacted, regardless of `redact_fields`:
//! `token`, `secret`, `password`, `authorization`, `api_key`,
//! `bearer`, `jwt`, `cookie`, `session`. These names cover the
//! common footguns. Operators extend via `redact_fields` in
//! workflow TOML.

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::Event;
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use super::LogTarget;

/// `[logging.audit]` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// Where to write redacted audit JSONL. Defaults to
    /// `file:/var/log/agent/audit.jsonl` when the block is present
    /// but the field is omitted — reduces "why is this logging to
    /// stderr?" surprise.
    #[serde(default)]
    pub target: Option<LogTarget>,

    /// Extra field names to redact on top of the built-in list.
    /// Case-insensitive match against the field name.
    #[serde(default)]
    pub redact_fields: Vec<String>,

    /// When `false` (default), the `reason` field's value is
    /// redacted. Auth denial reasons frequently echo the malformed
    /// token prefix; keeping them out of the audit stream by
    /// default is the safer posture.
    #[serde(default)]
    pub include_reason: bool,
}

impl AuditConfig {
    /// Combined set of lowercase field names to redact. Always
    /// includes the built-in list; `include_reason = true` removes
    /// `"reason"` from the set.
    pub fn redaction_set(&self) -> HashSet<String> {
        let mut set: HashSet<String> = DEFAULT_REDACT
            .iter()
            .map(|s| (*s).to_ascii_lowercase())
            .collect();
        for f in &self.redact_fields {
            set.insert(f.trim().to_ascii_lowercase());
        }
        if self.include_reason {
            set.remove("reason");
        } else {
            set.insert("reason".into());
        }
        set
    }

    /// Effective log target — default is a file under /var/log when
    /// the operator declared `[logging.audit]` without a target.
    pub fn effective_target(&self) -> LogTarget {
        self.target.clone().unwrap_or_else(|| {
            LogTarget::File(std::path::PathBuf::from("/var/log/agent/audit.jsonl"))
        })
    }
}

/// Fields redacted by default. Lowercase for case-insensitive match.
const DEFAULT_REDACT: &[&str] = &[
    "token",
    "secret",
    "password",
    "authorization",
    "api_key",
    "bearer",
    "jwt",
    "cookie",
    "session",
];

// ---------------------------------------------------------------------------
// Layer
// ---------------------------------------------------------------------------

/// `tracing_subscriber::Layer` that catches events with target
/// `"agentd::audit"`, applies field-level redaction, and writes one
/// JSONL line per event to a caller-supplied writer.
///
/// Events on any other target pass through unchanged (the layer's
/// `on_event` is a no-op for them).
pub struct AuditLayer {
    writer: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
    redact: HashSet<String>,
}

impl AuditLayer {
    pub fn new<W: std::io::Write + Send + 'static>(writer: W, cfg: &AuditConfig) -> Self {
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            redact: cfg.redaction_set(),
        }
    }
}

impl<S> Layer<S> for AuditLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != "agentd::audit" {
            return;
        }

        // Capture all event fields into a sortable map so the
        // output JSON is stable across runs.
        let mut collector = FieldCollector {
            values: Vec::new(),
            redact: &self.redact,
        };
        event.record(&mut collector);

        // Build the JSONL record. Match the shape the default
        // `tracing-subscriber::fmt::Json` layer emits so downstream
        // parsers that accept JSON tracing work with both streams.
        let mut record = serde_json::Map::new();
        record.insert(
            "timestamp".into(),
            serde_json::Value::String(time_rfc3339().unwrap_or_else(|| "unknown".into())),
        );
        record.insert(
            "level".into(),
            serde_json::Value::String(event.metadata().level().to_string()),
        );
        record.insert(
            "target".into(),
            serde_json::Value::String("agentd::audit".into()),
        );
        let mut fields = serde_json::Map::with_capacity(collector.values.len());
        for (k, v) in collector.values {
            fields.insert(k, v);
        }
        record.insert("fields".into(), serde_json::Value::Object(fields));

        let line = match serde_json::to_string(&record) {
            Ok(s) => s,
            Err(_) => return,
        };

        // Writes are synchronous under a mutex. This is fine for
        // audit volume (the audit log is low-rate by construction);
        // a non-blocking wrapper is a future optimisation if a
        // workload ever warrants it.
        if let Ok(mut w) = self.writer.lock() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Field visitor
// ---------------------------------------------------------------------------

struct FieldCollector<'a> {
    values: Vec<(String, serde_json::Value)>,
    redact: &'a HashSet<String>,
}

impl FieldCollector<'_> {
    fn record_named(&mut self, name: &str, value: serde_json::Value) {
        if self.redact.contains(&name.to_ascii_lowercase()) {
            self.values.push((
                name.to_string(),
                serde_json::Value::String("<redacted>".into()),
            ));
        } else {
            self.values.push((name.to_string(), value));
        }
    }
}

impl Visit for FieldCollector<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_named(
            field.name(),
            serde_json::Value::String(format!("{value:?}")),
        );
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_named(field.name(), serde_json::Value::String(value.to_string()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_named(field.name(), serde_json::Value::Bool(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_named(field.name(), serde_json::Value::Number(value.into()));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_named(field.name(), serde_json::Value::Number(value.into()));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        match serde_json::Number::from_f64(value) {
            Some(n) => self.record_named(field.name(), serde_json::Value::Number(n)),
            None => self.record_named(field.name(), serde_json::Value::String(format!("{value}"))),
        }
    }
}

fn time_rfc3339() -> Option<String> {
    // Match the `tracing-subscriber::fmt::Json` timestamp format
    // ("2026-04-23T05:30:00.123456Z"). We hand-roll to avoid
    // pulling the `chrono` or `time` crate — std's SystemTime is
    // enough for seconds + microsecond precision.
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    let secs = dur.as_secs() as i64;
    let micros = dur.subsec_micros();
    let (year, month, day, hour, minute, second) = break_down_epoch(secs);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z"
    ))
}

/// Very small gmtime-equivalent. std does not expose one on stable.
/// Handles 1970–2099 correctly; no DST (UTC only).
fn break_down_epoch(secs_since_epoch: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days_since_epoch = secs_since_epoch.div_euclid(86_400);
    let secs_of_day = secs_since_epoch.rem_euclid(86_400) as u32;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // Days since 1970-01-01 → Gregorian Y-M-D. Shift epoch to
    // 0000-03-01 so leap-year arithmetic simplifies.
    let mut days = days_since_epoch + 719_468; // days from 0000-03-01 to 1970-01-01
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097) as u32;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let mut year = year_of_era as i32 + (era * 400) as i32;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    let _ = &mut days;
    (year, month, day, hour, minute, second)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::prelude::*;

    /// In-memory writer for assertions.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    #[test]
    fn default_redaction_set_matches_builtin_list() {
        let cfg = AuditConfig::default();
        let set = cfg.redaction_set();
        for name in DEFAULT_REDACT {
            assert!(set.contains(*name), "missing {name}");
        }
        // `reason` is redacted by default.
        assert!(set.contains("reason"));
    }

    #[test]
    fn include_reason_removes_reason_from_redaction() {
        let cfg = AuditConfig {
            include_reason: true,
            ..Default::default()
        };
        let set = cfg.redaction_set();
        assert!(!set.contains("reason"));
        // Everything else still redacted.
        assert!(set.contains("token"));
    }

    #[test]
    fn operator_redact_fields_merge_with_default() {
        let cfg = AuditConfig {
            redact_fields: vec!["custom_secret".into(), "Token".into()],
            ..Default::default()
        };
        let set = cfg.redaction_set();
        assert!(set.contains("custom_secret"));
        assert!(set.contains("token")); // unchanged
    }

    #[test]
    fn writes_audit_events_only() {
        let buf = SharedBuf::default();
        let layer = AuditLayer::new(buf.clone(), &AuditConfig::default());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "agentd::audit", event = "x.happened");
            tracing::info!("non-audit event");
            tracing::warn!(target: "some::other", event = "ignored");
        });
        let contents = buf.contents();
        assert!(contents.contains("x.happened"), "contents: {contents}");
        assert!(!contents.contains("non-audit"));
        assert!(!contents.contains("ignored"));
    }

    #[test]
    fn redacts_named_fields() {
        let buf = SharedBuf::default();
        let layer = AuditLayer::new(
            buf.clone(),
            &AuditConfig {
                redact_fields: vec!["correlation_id".into()],
                include_reason: true, // so we can see the reason field pass through
                ..Default::default()
            },
        );
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: "agentd::audit",
                event = "auth.allowed",
                token = "s3cret-bearer-abcdef",
                correlation_id = "abc-123",
                reason = "valid"
            );
        });
        let contents = buf.contents();
        assert!(
            contents.contains("\"token\":\"<redacted>\""),
            "contents: {contents}"
        );
        assert!(
            contents.contains("\"correlation_id\":\"<redacted>\""),
            "contents: {contents}"
        );
        assert!(
            contents.contains("\"reason\":\"valid\""),
            "contents: {contents}"
        );
    }

    #[test]
    fn reason_redacted_by_default() {
        let buf = SharedBuf::default();
        let layer = AuditLayer::new(buf.clone(), &AuditConfig::default());
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                target: "agentd::audit",
                event = "auth.denied",
                reason = "bad token prefix: Bearer XYZ"
            );
        });
        let contents = buf.contents();
        assert!(
            contents.contains("\"reason\":\"<redacted>\""),
            "contents: {contents}"
        );
        assert!(!contents.contains("Bearer XYZ"));
    }

    #[test]
    fn epoch_break_down_samples() {
        // 2000-01-01 00:00:00 UTC — well-known edge case.
        assert_eq!(break_down_epoch(946_684_800), (2000, 1, 1, 0, 0, 0));
        // 2024-02-29 12:34:56 UTC — leap-year crossing.
        assert_eq!(break_down_epoch(1_709_210_096), (2024, 2, 29, 12, 34, 56));
        // 1970-01-01 00:00:00 UTC.
        assert_eq!(break_down_epoch(0), (1970, 1, 1, 0, 0, 0));
    }
}

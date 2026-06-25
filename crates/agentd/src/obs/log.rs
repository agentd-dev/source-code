//! Hand-rolled JSON-lines logger. ~150 lines reusing the `serde_json`
//! serializer — deliberately not `tracing` (its implicit async span context
//! is moot for a processes-plus-threads design, and the process tree gives
//! us correlation for free). RFC 0010 §default-logging.
//!
//! One NDJSON event per line to **stderr** (stdout is reserved for the
//! agent's result). The canonical line schema (RFC 0010 §line-schema) is:
//! `ts level event run_id agent_id agent_path comp pid [span_id parent_span_id
//! trace_id] [dur_ms] [err] <event-specific>`.

use serde_json::{Map, Value};
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Trace => "trace",
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }

    /// Parse a `--log-level` value; unknown → None (caller decides default).
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "trace" => Some(Level::Trace),
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warn),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

/// Which component is emitting. Part of the correlation tuple.
#[derive(Debug, Clone, Copy)]
pub enum Comp {
    Supervisor,
    Agent,
    Mcp,
    Intel,
}

impl Comp {
    fn as_str(self) -> &'static str {
        match self {
            Comp::Supervisor => "supervisor",
            Comp::Agent => "agent",
            Comp::Mcp => "mcp",
            Comp::Intel => "intel",
        }
    }
}

/// The correlation context stamped on every line. Children inherit `run_id`
/// and `trace_id` and extend `agent_path` (the cheap subtree-query superpower:
/// an `agent_path` prefix selects a subtree with no backend join). RFC 0010
/// §tree-correlation.
#[derive(Debug, Clone)]
pub struct LogCtx {
    pub run_id: String,
    pub agent_id: String,
    pub agent_path: String,
    pub comp: Comp,
    pub pid: u32,
    pub trace_id: Option<String>,
}

/// A logger bound to one [`LogCtx`]. Cheap to clone (clones the ctx); writes
/// serialize behind a process-global stderr mutex so lines never interleave.
pub struct Logger {
    ctx: LogCtx,
    min: Level,
}

// One lock so concurrent threads in a process don't interleave partial lines.
static STDERR_LOCK: Mutex<()> = Mutex::new(());

impl Logger {
    pub fn new(ctx: LogCtx, min: Level) -> Self {
        Logger { ctx, min }
    }

    pub fn ctx(&self) -> &LogCtx {
        &self.ctx
    }

    /// Emit one event. `fields` should be a JSON object; its keys are merged
    /// after the canonical fields (event-specific data). Non-object `fields`
    /// is ignored. Below `min` level: dropped cheaply.
    pub fn event(&self, level: Level, event: &str, fields: Value) {
        if level < self.min {
            return;
        }
        let mut m = Map::new();
        m.insert("ts".into(), Value::String(rfc3339_millis(SystemTime::now())));
        m.insert("level".into(), Value::String(level.as_str().into()));
        m.insert("event".into(), Value::String(event.into()));
        m.insert("run_id".into(), Value::String(self.ctx.run_id.clone()));
        m.insert("agent_id".into(), Value::String(self.ctx.agent_id.clone()));
        m.insert("agent_path".into(), Value::String(self.ctx.agent_path.clone()));
        m.insert("comp".into(), Value::String(self.ctx.comp.as_str().into()));
        m.insert("pid".into(), Value::Number(self.ctx.pid.into()));
        if let Some(tid) = &self.ctx.trace_id {
            m.insert("trace_id".into(), Value::String(tid.clone()));
        }
        if let Value::Object(extra) = fields {
            for (k, v) in extra {
                m.insert(k, v);
            }
        }
        // Build the whole line, then one locked write.
        let mut line = serde_json::to_vec(&Value::Object(m)).unwrap_or_else(|_| b"{}".to_vec());
        line.push(b'\n');
        let _guard = STDERR_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = std::io::stderr().write_all(&line);
    }

    pub fn info(&self, event: &str, fields: Value) {
        self.event(Level::Info, event, fields);
    }
    pub fn warn(&self, event: &str, fields: Value) {
        self.event(Level::Warn, event, fields);
    }
    pub fn error(&self, event: &str, fields: Value) {
        self.event(Level::Error, event, fields);
    }
    pub fn debug(&self, event: &str, fields: Value) {
        self.event(Level::Debug, event, fields);
    }
}

/// Format a `SystemTime` as RFC 3339 UTC with millisecond precision, with no
/// date-library dependency. Uses Howard Hinnant's `civil_from_days`
/// algorithm. Pre-epoch times clamp to the epoch (we never log them).
pub fn rfc3339_millis(t: SystemTime) -> String {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();

    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    let ss = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Days since 1970-01-01 → (year, month, day). Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rfc3339_known_timestamps() {
        // 0 -> the epoch.
        assert_eq!(rfc3339_millis(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // 1_700_000_000 = 2023-11-14T22:13:20Z (a well-known round value).
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(rfc3339_millis(t), "2023-11-14T22:13:20.000Z");
        // millis are rendered.
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(rfc3339_millis(t), "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn leap_year_day() {
        // 2024-02-29 is day 19782 since epoch.
        let t = UNIX_EPOCH + Duration::from_secs(19_782 * 86_400);
        assert_eq!(&rfc3339_millis(t)[..10], "2024-02-29");
    }

    #[test]
    fn level_ordering_filters() {
        assert!(Level::Debug < Level::Info);
        assert!(Level::Error > Level::Warn);
    }
}

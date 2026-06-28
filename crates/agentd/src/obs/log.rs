// SPDX-License-Identifier: Apache-2.0
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
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
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
#[derive(Clone)]
pub struct Logger {
    ctx: LogCtx,
    min: Level,
    log_content: bool,
}

// One lock so concurrent threads in a process don't interleave partial lines.
static STDERR_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// The bounded in-memory event ring (RFC 0016 §7.2). A projection of the same
// stderr stream — identical lines, identical closed vocabulary (RFC 0010
// §3.2/§3.3) — captured into a fixed-size ring the `agentd://events` resource
// drains with an `?after=<seq>` cursor (the self-MCP server reads it; this
// module only owns the store). NOT a second telemetry path: stderr stays the
// source of truth; the ring is the live-tail convenience.
//
// It is installed only when serving wants it (the supervisor calls
// [`install_event_ring`] once at startup); without that, capture is a single
// relaxed atomic load that short-circuits, so the default build pays nothing.
// The ring is lossy and bounded by design (§8.4): an overrun drops the oldest
// and bumps `dropped`, never blocking — a slow/dead subscriber can never
// back-pressure the supervisor.

/// Envelope version for the `agentd://events` read body (RFC 0016 §7.2/§8.1).
/// Bumped only on a breaking change to the `{oldest_seq,newest_seq,dropped,
/// events}` envelope — NOT the line schema (RFC 0010 owns + versions that).
pub const EVENTS_SCHEMA: &str = "1.0";

/// Default ring capacity (`AGENTD_EVENTS_RING`, RFC 0016 §7.2/§11): the last N
/// emitted lines held in memory. Bounds memory on a slow subscriber.
pub const EVENTS_RING_DEFAULT: usize = 1024;

/// One captured line plus its monotonic ring `seq` (the only field added over
/// the RFC 0010 §3.2 line — the cursor key the subscriber advances).
struct RingEntry {
    seq: u64,
    /// The captured `level` (cheap prefix-filterable without re-parsing).
    level: &'static str,
    /// The captured `event` name (cheap prefix-filterable without re-parsing).
    event: String,
    /// The full RFC 0010 §3.2 line object (the `seq` is added on read).
    line: Value,
}

/// A fixed-capacity ring of the last `cap` emitted lines. Lossy oldest-evicted;
/// `dropped` counts lines evicted to date (a subscriber whose `after` predates
/// `oldest_seq` learns it fell behind and re-baselines). RFC 0016 §7.2.
struct EventRing {
    buf: VecDeque<RingEntry>,
    cap: usize,
    /// Total lines evicted since start (monotonic; surfaced as `dropped`).
    dropped: u64,
}

impl EventRing {
    fn new(cap: usize) -> EventRing {
        // A zero cap would make every push an immediate eviction; clamp to 1 so
        // the ring always holds at least the newest line.
        let cap = cap.max(1);
        EventRing {
            buf: VecDeque::with_capacity(cap),
            cap,
            dropped: 0,
        }
    }

    /// Append a line, evicting the oldest (and bumping `dropped`) on overrun.
    fn push(&mut self, entry: RingEntry) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.buf.push_back(entry);
    }
}

/// The process-global ring. `None` until [`install_event_ring`] is called; a
/// relaxed load gates the capture hot path so the default build is free.
static EVENT_RING: Mutex<Option<EventRing>> = Mutex::new(None);
/// Cheap presence flag so the logging hot path avoids the mutex when no ring is
/// installed (the overwhelmingly common case). Set once at install.
static RING_INSTALLED: AtomicU64 = AtomicU64::new(0);
/// Monotonic ring sequence — the cursor key. Shared across all loggers in the
/// process so every captured line gets a globally-ordered `seq`.
static RING_SEQ: AtomicU64 = AtomicU64::new(0);
/// Set on every ring push; the served `agentd://events` resource coalesces this
/// into one `notifications/resources/updated` per tick (RFC 0016 §7.2: "a small
/// coalescing batch"). A flag, not a callback, keeps this `obs` layer free of any
/// self-MCP server type — the server polls + clears it. Non-blocking (§8.4).
static EVENTS_DIRTY: AtomicU64 = AtomicU64::new(0);

/// Take-and-clear the "new events since last check" flag — the served
/// `agentd://events` resource calls this on its coalescing tick to decide whether
/// to fire a `notifications/resources/updated`. Returns `true` if any line was
/// captured since the last call. RFC 0016 §7.2.
pub fn take_events_dirty() -> bool {
    EVENTS_DIRTY.swap(0, Ordering::Relaxed) != 0
}

/// Install the bounded event ring with capacity `cap` (RFC 0016 §7.2). Called
/// once by the supervisor when the served `agentd://events` resource is wanted
/// (gated by `--serve-mcp` + the `events` feature at the call site). Idempotent
/// — a second call resizes/clears. Never fatal (telemetry never crashes the
/// run, §8.4).
pub fn install_event_ring(cap: usize) {
    let mut g = EVENT_RING.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some(EventRing::new(cap));
    RING_INSTALLED.store(1, Ordering::Relaxed);
}

/// A snapshot of the ring window an `agentd://events?after=<seq>` read returns:
/// the entries with `seq > after` (after optional level/event-prefix filtering),
/// plus the ring's current window bounds and cumulative `dropped`. RFC 0016 §7.2.
pub struct EventWindow {
    pub events: Vec<Value>,
    pub oldest_seq: u64,
    pub newest_seq: u64,
    pub dropped: u64,
}

/// Drain the ring into an [`EventWindow`] for a cursor read. Returns the entries
/// with `seq > after`, capped at `limit` (oldest-first), each with its `seq`
/// folded into the RFC 0010 §3.2 line object. `level`/`event_prefixes` are the
/// optional §7.3 server-side filters (a cheap prefix match over the held lines —
/// no query engine). `None` when no ring is installed (the resource 404s at the
/// server). RFC 0016 §7.2/§7.3.
pub fn read_event_window(
    after: u64,
    limit: usize,
    level: Option<&str>,
    event_prefixes: &[&str],
) -> Option<EventWindow> {
    if RING_INSTALLED.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let g = EVENT_RING.lock().unwrap_or_else(|e| e.into_inner());
    let ring = g.as_ref()?;
    let oldest_seq = ring.buf.front().map(|e| e.seq).unwrap_or(0);
    let newest_seq = ring.buf.back().map(|e| e.seq).unwrap_or(0);
    let mut events = Vec::new();
    for entry in ring.buf.iter() {
        if entry.seq <= after {
            continue;
        }
        if let Some(want) = level
            && entry.level != want
        {
            continue;
        }
        if !event_prefixes.is_empty() && !event_prefixes.iter().any(|p| entry.event.starts_with(p))
        {
            continue;
        }
        // Fold the ring `seq` into the line object (the only added field).
        let mut line = match &entry.line {
            Value::Object(m) => m.clone(),
            _ => Map::new(),
        };
        line.insert("seq".into(), Value::Number(entry.seq.into()));
        events.push(Value::Object(line));
        if events.len() >= limit {
            break;
        }
    }
    Some(EventWindow {
        events,
        oldest_seq,
        newest_seq,
        dropped: ring.dropped,
    })
}

/// Capture one already-assembled line into the ring (a no-op when none is
/// installed). Pulls `level`/`event` off the object for cheap filterable
/// metadata, mints a `seq`, and pushes — lossy oldest-evicted, never blocking.
/// Best-effort: a poisoned lock is recovered, never fatal (§8.4).
fn capture_to_ring(level: &'static str, event: &str, line: &Value) {
    if RING_INSTALLED.load(Ordering::Relaxed) == 0 {
        return;
    }
    let seq = RING_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let mut g = EVENT_RING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ring) = g.as_mut() {
        ring.push(RingEntry {
            seq,
            level,
            event: event.to_string(),
            line: line.clone(),
        });
        // Mark the ring dirty so the served resource coalesces a notify (§7.2).
        EVENTS_DIRTY.store(1, Ordering::Relaxed);
    }
}

impl Logger {
    pub fn new(ctx: LogCtx, min: Level) -> Self {
        Logger {
            ctx,
            min,
            log_content: false,
        }
    }

    /// Opt into content capture (RFC 0010 §2.9): callers that log tool
    /// args/results consult [`Logger::content_capture`]. Off by default.
    pub fn with_content(mut self, on: bool) -> Self {
        self.log_content = on;
        self
    }

    /// Whether this logger may record tool args/results (not just lengths).
    pub fn content_capture(&self) -> bool {
        self.log_content
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
        m.insert(
            "ts".into(),
            Value::String(rfc3339_millis(SystemTime::now())),
        );
        m.insert("level".into(), Value::String(level.as_str().into()));
        m.insert("event".into(), Value::String(event.into()));
        m.insert("run_id".into(), Value::String(self.ctx.run_id.clone()));
        m.insert("agent_id".into(), Value::String(self.ctx.agent_id.clone()));
        m.insert(
            "agent_path".into(),
            Value::String(self.ctx.agent_path.clone()),
        );
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
        let value = Value::Object(m);
        // Project the line into the bounded `agentd://events` ring (RFC 0016
        // §7.2) — the same line, captured for the live-tail resource. A no-op
        // (one relaxed atomic load) unless a ring is installed. Best-effort:
        // capture never blocks and never fails the log write (§8.4).
        capture_to_ring(level.as_str(), event, &value);
        // Build the whole line, then one locked write.
        let mut line = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
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

/// Days since 1970-01-01 → (year, month, day). Hinnant's algorithm. Shared with
/// the cron `timer` (UTC field decomposition).
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
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

    fn ring_entry(seq: u64, level: &'static str, event: &str) -> RingEntry {
        RingEntry {
            seq,
            level,
            event: event.to_string(),
            line: serde_json::json!({"event": event, "level": level}),
        }
    }

    #[test]
    fn ring_evicts_oldest_and_counts_dropped() {
        // A 2-slot ring: pushing 3 lines drops exactly the oldest and bumps
        // `dropped` once (lossy-by-design, RFC 0016 §7.2/§8.4).
        let mut r = EventRing::new(2);
        r.push(ring_entry(1, "info", "loop.step"));
        r.push(ring_entry(2, "info", "loop.step"));
        assert_eq!(r.dropped, 0);
        r.push(ring_entry(3, "warn", "limit.exceeded"));
        assert_eq!(r.dropped, 1);
        // Oldest (seq 1) is gone; 2 and 3 remain.
        let seqs: Vec<u64> = r.buf.iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    #[test]
    fn ring_zero_cap_clamps_to_one() {
        // A 0 cap would make every push an immediate eviction; it clamps to 1 so
        // the ring always holds the newest line.
        let mut r = EventRing::new(0);
        r.push(ring_entry(1, "info", "a"));
        r.push(ring_entry(2, "info", "b"));
        assert_eq!(r.buf.len(), 1);
        assert_eq!(r.buf.back().unwrap().seq, 2);
        assert_eq!(r.dropped, 1);
    }

    #[test]
    fn install_then_read_window_with_cursor_and_filters() {
        // The ring is process-global, so this test owns it for its duration. It
        // installs a fresh ring, emits a few lines through a real Logger (the
        // capture path), then drains the window with the `?after` cursor and the
        // §7.3 level/event-prefix filters.
        install_event_ring(64);
        let base = RING_SEQ.load(Ordering::Relaxed); // cursor is global+monotonic
        let log = Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 1,
                trace_id: None,
            },
            Level::Trace,
        );
        log.info("loop.step", serde_json::json!({"step": 1}));
        log.warn("limit.exceeded", serde_json::json!({"limit": "steps"}));
        log.info("subagent.spawn", serde_json::json!({"node": 1}));

        // No filter: everything after `base` is returned, each carrying a `seq`.
        let w = read_event_window(base, 100, None, &[]).expect("ring installed");
        assert!(w.events.len() >= 3);
        assert!(w.events.iter().all(|e| e.get("seq").is_some()));
        assert!(w.newest_seq >= w.oldest_seq);

        // Level filter: only the warn line.
        let w = read_event_window(base, 100, Some("warn"), &[]).expect("ring");
        assert!(w.events.iter().all(|e| e["level"] == "warn"));
        assert!(w.events.iter().any(|e| e["event"] == "limit.exceeded"));

        // Event-prefix filter: only `subagent.*`.
        let w = read_event_window(base, 100, None, &["subagent."]).expect("ring");
        assert!(
            w.events
                .iter()
                .all(|e| e["event"].as_str().unwrap().starts_with("subagent."))
        );

        // `limit` caps the slice oldest-first.
        let w = read_event_window(base, 1, None, &[]).expect("ring");
        assert_eq!(w.events.len(), 1);
    }
}

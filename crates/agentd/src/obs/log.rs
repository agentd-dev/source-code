// SPDX-License-Identifier: AGPL-3.0-only
//! Hand-rolled JSON-lines logger. ~150 lines reusing the `serde_json`
//! serializer — deliberately not `tracing` (its implicit async span context
//! is moot for a processes-plus-threads design, and the process tree gives
//! us correlation for free).
//!
//! One NDJSON event per line to **stderr** (stdout is reserved for the
//! agent's result). Every line carries `ts level event run_id agent_id
//! agent_path comp pid`, then `trace_id`, `span_id` and `parent_span_id` when
//! known, then the event's own fields. `run_id` is the process's; a workflow
//! run is the event field `run`.
//!
//! The trace is the line's unit of work's: a line about a run, a step, a turn,
//! a start firing, an inbox event, a subagent or a delivery is written through
//! [`Logger::in_trace`] and carries that unit's trace id and span; a line
//! about the process (boot, reload, drain, the store) carries the process
//! trace ([`crate::obs::trace`]).

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
/// and extend `agent_path` (the cheap subtree-query superpower: an
/// `agent_path` prefix selects a subtree with no backend join), and take the
/// trace of the unit of work they serve.
#[derive(Debug, Clone)]
pub struct LogCtx {
    pub run_id: String,
    pub agent_id: String,
    pub agent_path: String,
    pub comp: Comp,
    pub pid: u32,
    pub trace_id: Option<String>,
    /// The span the line is about, and its parent.
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    /// The trace's flags (`01` sampled, `00` not). Not written on the line:
    /// it is what a carrier built from the line goes on with — the `_runtime`
    /// stream's event — so an unsampled trace stays unsampled.
    pub trace_flags: Option<String>,
}

impl LogCtx {
    /// Set the trace fields to `t`'s.
    pub fn set_trace(&mut self, t: &crate::obs::trace::TraceContext) {
        self.trace_id = Some(t.trace_id.clone());
        self.span_id = Some(t.span_id.clone());
        self.parent_span_id = t.parent_span_id.clone();
        self.trace_flags = Some(t.flags.clone());
    }

    /// The span the line is in, when it is in one.
    pub fn trace(&self) -> Option<crate::obs::trace::TraceContext> {
        Some(crate::obs::trace::TraceContext {
            trace_id: self.trace_id.clone()?,
            span_id: self.span_id.clone()?,
            parent_span_id: self.parent_span_id.clone(),
            flags: self.trace_flags.clone().unwrap_or_else(|| "01".into()),
            tracestate: None,
        })
    }
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
// The bounded in-memory event ring. A projection of the same stderr stream —
// identical lines, identical closed event vocabulary — captured into a
// fixed-size ring the `debug.events` command op drains with an `after` cursor
// (the A2A command surface reads it; this module only owns the store). NOT a
// second telemetry path: stderr stays the source of truth; the ring is the
// live-tail convenience.
//
// It is installed only when the debug tail is wanted (`a2a.introspection.enabled`
// at startup, or turned on later by `admin.set` or a reload); without that, capture is a single relaxed atomic load that short-circuits, so
// the default build pays nothing.
// The ring is lossy and bounded by design: an overrun drops the oldest and bumps
// `dropped`, never blocking — a slow or dead subscriber can never back-pressure
// the supervisor.

/// Default ring capacity, overridable with `observability.events_ring`
/// (`--events-ring`): the last N emitted lines held in memory. Bounds memory on
/// a slow subscriber.
pub const EVENTS_RING_DEFAULT: usize = 1024;

/// One captured line plus its monotonic ring `seq` — the only field added over
/// the emitted line, and the cursor key the subscriber advances.
struct RingEntry {
    seq: u64,
    /// The captured `level` (cheap prefix-filterable without re-parsing).
    level: &'static str,
    /// The captured `event` name (cheap prefix-filterable without re-parsing).
    event: String,
    /// The full emitted line object (the `seq` is added on read).
    line: Value,
}

/// A fixed-capacity ring of the last `cap` emitted lines. Lossy oldest-evicted;
/// `dropped` counts lines evicted to date (a subscriber whose `after` predates
/// `oldest_seq` learns it fell behind and re-baselines).
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
/// Set on every ring push, so a reader can coalesce a burst into one
/// notification per tick rather than notifying per captured line. A flag, not a
/// callback, keeps this `obs` layer free of any server type — the reader polls
/// and clears it. Non-blocking, so a push never waits on a subscriber.
static EVENTS_DIRTY: AtomicU64 = AtomicU64::new(0);

/// Take-and-clear the "new events since last check" flag — a reader calls this
/// on its coalescing tick to decide whether to notify subscribers. Returns
/// `true` if any line was captured since the last call.
pub fn take_events_dirty() -> bool {
    EVENTS_DIRTY.swap(0, Ordering::Relaxed) != 0
}

/// Install the bounded event ring with capacity `cap`. Called by the runtime
/// when the live log tail is wanted — `a2a.introspection.enabled` at startup,
/// or turned on later by `admin.set` or a reload.
///
/// Idempotent: a ring already installed with this capacity is kept, lines and
/// cursor both, because introspection can be switched on from several places
/// and an operator tailing the log should not lose it to a second switch. A
/// different capacity replaces the ring. Never fatal: telemetry must not crash
/// the run, so a poisoned lock is recovered rather than propagated.
pub fn install_event_ring(cap: usize) {
    let mut g = EVENT_RING.lock().unwrap_or_else(|e| e.into_inner());
    if g.as_ref().is_some_and(|r| r.cap == cap.max(1)) {
        return;
    }
    *g = Some(EventRing::new(cap));
    RING_INSTALLED.store(1, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// The runtime-events tap. The daemon emits a large dotted event vocabulary and
// exposes almost none of it to ITSELF: "the breaker tripped" is a log line, not
// something a workflow can react to. This tap turns a selected subset into
// appends on a declared stream, so remediation, paging and SLO accounting
// become ordinary `{kind: stream}` start nodes with the retention, subject
// globbing, filters and dedup those already have.
//
// It is NOT the event ring. The ring is an explicitly lossy oldest-evicted
// buffer, installed only when introspection is on (`a2a.introspection.enabled`,
// at startup or turned on later); teeing it into a durable stream would produce silent
// gaps in exactly the consumer being sold, and in the default deployment —
// where the ring is never installed at all — would do nothing. This taps the
// emission itself — every `log.info`/`warn`/`error` call site — and is opt-in
// per family, carrying metadata rather than payloads.
//
// Volume is the real hazard, because the refusals that matter most arrive in
// storms precisely when the constrained resource is the thing being written
// to. Three things bound it: the queue is FIXED and counts what it drops
// (keeping the oldest, so the start of a storm survives and the count says how
// big it got), a family may be marked `sampled` to contribute at 1/N, and the
// drain goes through `append_event`, which sheds under pressure like every
// other admission.

/// The families a runtime event can belong to — the segment before the first
/// dot in an event name. A closed vocabulary so `include: [pressur]` is a
/// startup error rather than a filter that silently matches nothing — which
/// is also why a family nothing emits any more is removed rather than kept.
/// `families_cover_the_emitted_vocabulary` keeps this honest against the tree
/// in both directions.
pub const EVENT_FAMILIES: &[&str] = &[
    "a2a",
    "admin",
    "agent",
    "audit",
    "auth",
    "breaker",
    "budget",
    "cgroup",
    "child",
    "company",
    "config",
    "context",
    "correlate",
    "delegation",
    "drain",
    "exec",
    "freshness",
    "goal",
    "http",
    "human",
    "identity",
    "inbox",
    "instance",
    "instruction",
    "intel",
    "knowledge",
    "lifecycle",
    "limit",
    "loop",
    "mcp",
    "message",
    "metrics",
    "nested",
    "otel",
    "payment",
    "plan",
    "preflight",
    "pressure",
    "proc",
    "prompt",
    "readback",
    "registry",
    "restore",
    "root",
    "run",
    "security",
    "signal",
    "skills",
    "start",
    "step",
    "store",
    "stream",
    "subagent",
    "test",
    "timer",
    "tool",
    "turn",
    "wait",
    "webhook",
    "webhooks",
    "workflow",
];

/// One event awaiting its append: which stream, the subject (the event name),
/// the fields, and the span the line was in — the event's `traceparent`, so a
/// run that consumes it continues the trace of what it reports on.
pub struct TappedEvent {
    pub stream: String,
    pub subject: String,
    pub data: Value,
    pub trace: Option<crate::obs::trace::TraceContext>,
}

/// How often a `sampled` family actually contributes.
const SAMPLE_EVERY: u64 = 16;

struct RuntimeTap {
    /// Where selected runtime events land.
    stream: String,
    /// Families taken in full.
    all: Vec<String>,
    /// Families taken at 1-in-[`SAMPLE_EVERY`] — the high-rate ones, which
    /// cannot share a list with a once-a-week `config.reloaded`.
    sampled: Vec<String>,
    queue: VecDeque<TappedEvent>,
    cap: usize,
    /// Dropped because the queue was full. Surfaced on the next drain, so a
    /// storm is visible as a fact rather than as silence.
    dropped: u64,
    /// Per-family counters driving the sampling decision.
    seen: u64,
}

static RUNTIME_TAP: Mutex<Option<RuntimeTap>> = Mutex::new(None);
/// Cheap presence flag — the logging hot path avoids the mutex entirely when
/// no tap is armed, which is the default.
static TAP_ARMED: AtomicU64 = AtomicU64::new(0);

/// Arm the tap. `all` and `sampled` are family names; `cap` bounds the queue.
/// Idempotent — a second call replaces the selection and clears the queue.
pub fn install_runtime_tap(stream: &str, all: Vec<String>, sampled: Vec<String>, cap: usize) {
    let mut g = RUNTIME_TAP.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some(RuntimeTap {
        stream: stream.to_string(),
        all,
        sampled,
        queue: VecDeque::new(),
        cap: cap.max(1),
        dropped: 0,
        seen: 0,
    });
    TAP_ARMED.store(1, Ordering::Relaxed);
}

/// Whether any tap is armed (the cheap check the audit sink also uses).
pub fn runtime_tap_armed() -> bool {
    TAP_ARMED.load(Ordering::Relaxed) != 0
}

/// Queue one event for a stream directly, bypassing family selection. The
/// audit sink uses this: audit records are chosen by `audit.sink`, not by the
/// runtime-event family list, and they go to their own stream. `trace` is the
/// span the record is in.
pub fn tap_direct(
    stream: &str,
    subject: &str,
    data: Value,
    trace: Option<crate::obs::trace::TraceContext>,
) {
    if !runtime_tap_armed() {
        return;
    }
    let mut g = RUNTIME_TAP.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = g.as_mut() {
        push_bounded(t, stream.to_string(), subject.to_string(), data, trace);
    }
}

fn push_bounded(
    t: &mut RuntimeTap,
    stream: String,
    subject: String,
    data: Value,
    trace: Option<crate::obs::trace::TraceContext>,
) {
    if t.queue.len() >= t.cap {
        // Drop the NEW one and keep the oldest: under a storm the first
        // refusals say what started it, and the count says how bad it got.
        t.dropped = t.dropped.saturating_add(1);
        return;
    }
    t.queue.push_back(TappedEvent {
        stream,
        subject,
        data,
        trace,
    });
}

/// Set while the drain is appending. The appends themselves log — a stream at
/// its retention ceiling emits `stream.trimmed` on EVERY append — so without
/// this the tap would feed itself: one appended event produces a log line that
/// becomes the next tick's appended event, forever, and fastest exactly when
/// the stream is already full. Telemetry must not observe its own plumbing.
static TAP_DRAINING: AtomicU64 = AtomicU64::new(0);

/// Suspend capture for the duration of the returned guard.
pub fn tap_drain_guard() -> TapDrainGuard {
    TAP_DRAINING.store(1, Ordering::Relaxed);
    TapDrainGuard
}

pub struct TapDrainGuard;
impl Drop for TapDrainGuard {
    fn drop(&mut self) {
        TAP_DRAINING.store(0, Ordering::Relaxed);
    }
}

/// The capture hot path, called from [`Logger::event`] for every emission.
/// `ctx` is the logger's: the span the line is in.
fn capture_to_tap(event: &str, line: &Value, ctx: &LogCtx) {
    if !runtime_tap_armed() || TAP_DRAINING.load(Ordering::Relaxed) != 0 {
        return;
    }
    let family = event.split('.').next().unwrap_or(event);
    let mut g = RUNTIME_TAP.lock().unwrap_or_else(|e| e.into_inner());
    let Some(t) = g.as_mut() else { return };
    let full = t.all.iter().any(|f| f == family);
    let sampled = !full && t.sampled.iter().any(|f| f == family);
    if !full && !sampled {
        return;
    }
    if sampled {
        t.seen = t.seen.wrapping_add(1);
        if t.seen % SAMPLE_EVERY != 0 {
            return;
        }
    }
    let stream = t.stream.clone();
    // Metadata, not payloads: the emitted line already excludes conversation
    // content unless `log_content` is on, and this carries it as-is so a
    // consumer reads the same fields an operator would see in the log.
    push_bounded(t, stream, event.to_string(), line.clone(), ctx.trace());
}

/// Take everything queued since the last drain, plus how many were dropped.
/// The reactor calls this once per tick and appends each to its stream.
pub fn drain_runtime_tap() -> (Vec<TappedEvent>, u64) {
    if !runtime_tap_armed() {
        return (Vec::new(), 0);
    }
    let mut g = RUNTIME_TAP.lock().unwrap_or_else(|e| e.into_inner());
    let Some(t) = g.as_mut() else {
        return (Vec::new(), 0);
    };
    let dropped = std::mem::take(&mut t.dropped);
    (t.queue.drain(..).collect(), dropped)
}

/// A snapshot of the ring window a `debug.events` read with an `after` cursor
/// returns: the entries with `seq > after` (after optional level/event-prefix
/// filtering), plus the ring's current window bounds and cumulative `dropped`.
pub struct EventWindow {
    pub events: Vec<Value>,
    pub oldest_seq: u64,
    pub newest_seq: u64,
    pub dropped: u64,
}

/// Drain the ring into an [`EventWindow`] for a cursor read. Returns the entries
/// with `seq > after`, capped at `limit` (oldest-first), each with its `seq`
/// folded into the emitted line object. `level`/`event_prefixes` are optional
/// server-side filters — a cheap prefix match over the held lines, not a query
/// engine. `None` when no ring is installed (the resource 404s at the server).
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
/// Best-effort: a poisoned lock is recovered, never fatal — telemetry must not
/// take down the run it is describing.
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
        // Mark the ring dirty so the served resource coalesces a notify.
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

    /// Opt into content capture: callers that log tool args/results consult
    /// [`Logger::content_capture`] first. Off by default, so args and results
    /// are recorded as lengths only unless an operator asks for the bodies.
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

    /// This logger for the lines about one unit of work — a run, a step, a
    /// turn, a start firing, a subagent: the same process context, with the
    /// unit's trace id and span in place of the process's. `None` (a unit
    /// whose record predates traces) keeps the process trace.
    pub fn in_trace(&self, t: Option<&crate::obs::trace::TraceContext>) -> Logger {
        let mut l = self.clone();
        if let Some(t) = t {
            l.ctx.set_trace(t);
        }
        l
    }

    /// Emit one event. `fields` should be a JSON object; its keys are merged
    /// after the canonical fields (event-specific data). Non-object `fields`
    /// is ignored. Below `min` level: dropped cheaply, but for the span links
    /// it names.
    pub fn event(&self, level: Level, event: &str, fields: Value) {
        // The spans the line names as `link` or `links` are links of the
        // span it is in, written or not: what a span links to does not
        // depend on the log level. A no-op unless spans are exported.
        crate::obs::otel::capture_links(event, &self.ctx, &fields);
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
        for (k, v) in [
            ("trace_id", &self.ctx.trace_id),
            ("span_id", &self.ctx.span_id),
            ("parent_span_id", &self.ctx.parent_span_id),
        ] {
            if let Some(v) = v {
                m.insert(k.into(), Value::String(v.clone()));
            }
        }
        if let Value::Object(extra) = fields {
            for (k, v) in extra {
                m.insert(k, v);
            }
        }
        let value = Value::Object(m);
        // Project the line into the bounded event ring — the same line,
        // captured for the `debug.events` live tail. A no-op (one relaxed atomic
        // load) unless a ring is installed. Best-effort: capture never blocks and
        // never fails the log write.
        capture_to_ring(level.as_str(), event, &value);
        // Project the line onto the runtime-events stream when one is armed and
        // this family was selected. A single relaxed atomic load otherwise.
        capture_to_tap(event, &value, &self.ctx);
        // Mirror to the OTLP logs exporter — a no-op (one atomic load) unless
        // `otel.logs` armed it. Best-effort, never blocks the write.
        crate::obs::otel::capture_log(
            crate::obs::otel::now_unix_nanos(),
            level.as_str(),
            event,
            &value,
        );
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

/// An RFC 3339 date-time — `2026-12-31T00:00:00Z`, a fraction and a
/// `±HH:MM` offset allowed, `T` and `Z` in either case — as epoch
/// milliseconds; `None` for anything else, or a time before the epoch.
pub fn rfc3339_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = s.get(from..to)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut at = 19;
    let mut millis = 0i64;
    if b[at] == b'.' {
        let start = at + 1;
        at = start;
        while at < b.len() && b[at].is_ascii_digit() {
            at += 1;
        }
        if at == start {
            return None;
        }
        let digits = &s[start..at.min(start + 3)];
        millis = digits.parse::<i64>().ok()? * 10i64.pow(3 - digits.len() as u32);
    }
    let offset_s = match &s[at..] {
        "Z" | "z" => 0,
        o if o.len() == 6 && matches!(o.as_bytes()[0], b'+' | b'-') && o.as_bytes()[3] == b':' => {
            let (oh, om) = (num(at + 1, at + 3)?, num(at + 4, at + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let v = oh * 3600 + om * 60;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // Days since the epoch of the civil date (Hinnant's `days_from_civil`).
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_s;
    u64::try_from(secs * 1000 + millis).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// An RFC 3339 time reads back as the instant `rfc3339_millis` writes,
    /// its offset applied; anything else is refused.
    #[test]
    fn an_rfc3339_time_is_read_as_its_instant() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_ms("2023-11-14T22:13:20.123Z"),
            Some(1_700_000_000_123)
        );
        assert_eq!(
            rfc3339_ms("2023-11-14t22:13:20.5z"),
            Some(1_700_000_000_500)
        );
        assert_eq!(
            rfc3339_ms("2026-10-08T12:20:00-07:00"),
            rfc3339_ms("2026-10-08T19:20:00Z")
        );
        assert_eq!(
            rfc3339_ms("2024-02-29T00:00:00+01:00"),
            Some(19_782 * 86_400_000 - 3_600_000)
        );
        let t = UNIX_EPOCH + Duration::from_millis(1_792_368_123_456);
        assert_eq!(rfc3339_ms(&rfc3339_millis(t)), Some(1_792_368_123_456));
        for bad in [
            "",
            "2026-12-31",
            "2026-12-31 00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-12-31T24:00:00Z",
            "2026-12-31T00:00:00",
            "2026-12-31T00:00:00.Z",
            "2026-12-31T00:00:00+0100",
            "1969-12-31T23:59:59Z",
            "２026-12-31T00:00:00Z",
        ] {
            assert_eq!(rfc3339_ms(bad), None, "{bad:?}");
        }
    }

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
        // `dropped` once — the ring is lossy by design.
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
        // level/event-prefix filters.
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
                span_id: None,
                parent_span_id: None,
                trace_flags: None,
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

    /// The family list is a CLOSED vocabulary an operator's config is
    /// validated against, so a family the tree emits but the list omits would
    /// make `include: [that]` a startup error for an event that really exists
    /// — and a family the list keeps after its last emitter is gone would
    /// validate `include: [that]` and match nothing, the very failure the
    /// closed list exists to prevent. Scanning the source keeps the list
    /// honest without anyone remembering to, in both directions.
    #[test]
    fn families_cover_the_emitted_vocabulary() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut missing: Vec<String> = Vec::new();
        let mut emitted: Vec<String> = Vec::new();
        for f in files {
            let src = std::fs::read_to_string(&f).unwrap_or_default();
            for needle in [".info(", ".warn(", ".error(", ".debug("] {
                let mut rest = src.as_str();
                while let Some(i) = rest.find(needle) {
                    rest = &rest[i + needle.len()..];
                    // The name may sit on the next line, where rustfmt puts a
                    // long call's arguments.
                    let Some(after) = rest.trim_start().strip_prefix('"') else {
                        continue;
                    };
                    rest = after;
                    let Some(end) = rest.find('"') else { break };
                    let name = &rest[..end];
                    // Event names are dotted lowercase identifiers; anything
                    // else is a different call that happens to look similar.
                    if name.is_empty()
                        || !name.chars().all(|c| {
                            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'
                        })
                    {
                        continue;
                    }
                    let family = name.split('.').next().unwrap_or(name);
                    if !EVENT_FAMILIES.contains(&family) && !missing.contains(&family.to_string()) {
                        missing.push(family.to_string());
                    }
                    if !emitted.contains(&family.to_string()) {
                        emitted.push(family.to_string());
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these event families are emitted but missing from EVENT_FAMILIES: {missing:?}"
        );
        let silent: Vec<&&str> = EVENT_FAMILIES
            .iter()
            .filter(|f| !emitted.iter().any(|e| e == *f))
            .collect();
        assert!(
            silent.is_empty(),
            "these event families are listed but nothing emits them: {silent:?}"
        );
    }

    /// Sorted and deduped, so the error message listing the known families
    /// reads as a reference rather than as whatever order they were added in.
    #[test]
    fn the_family_list_is_sorted_and_unique() {
        let mut sorted = EVENT_FAMILIES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), EVENT_FAMILIES);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! W3C Trace Context: `traceparent` and `tracestate`.
//!
//! **One trace per unit of work.** A unit is an inbox event — a start firing
//! of any kind, an A2A message, the `--prompt` — and the run or conversation
//! turn it becomes. Its trace is resolved once, where the event is written
//! ahead (`Runtime::accept_event_store`): continued from the unit that caused
//! it when there is one (a run that called `workflow.run`, sent a signal or a
//! message, or ran a child workflow), else minted with a random trace id. The
//! inbox event and the run record keep it, so a replayed event, a resumed run
//! and a restored subagent carry on in the trace they started in. Everything
//! the unit does carries it: the supervisor's log lines about it, the turn
//! workers and subagents it spawns (their spawn payload holds the unit's
//! `traceparent` and `tracestate`), every MCP request and every model call
//! they make, and the instance children and `exec` commands it starts.
//!
//! The process has a trace of its own for what belongs to no unit — boot,
//! reload, drain, the store, background MCP traffic: `observability.traceparent`
//! continued when one is given, else minted. Propagation is always on and
//! dependency-free; span export (OTLP) is the only `otel`-gated part.
//!
//! Each hop that sends a `traceparent` sends a span id of its own, a child of
//! the unit's span; the flags go on as they came (an upstream `00` stays
//! `00`), and so does `tracestate`, within the W3C limits.
//!
//! `traceparent` = `<version>-<32-hex trace-id>-<16-hex parent-id>-<2-hex
//! flags>`, lowercase. A version agentd does not know is read by its first
//! four fields; agentd sends version `00`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// At most this many list-members in a `tracestate` (W3C Trace Context §3.3.1).
pub const TRACESTATE_MAX_MEMBERS: usize = 32;
/// At most this many bytes of `tracestate` are stored or sent; a longer one is
/// dropped whole, never cut, so no vendor's entry arrives half-written.
pub const TRACESTATE_MAX_BYTES: usize = 512;

/// A span in a trace: the trace id, the span's own id, the span it is a child
/// of (when that is known), the sampling flags and the vendor `tracestate`.
///
/// Stored on the durable records of a unit of work (the inbox event, the run,
/// a subagent), so its serialized form is part of the store's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceContext {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
    /// `01` sampled, `00` not; the only flag W3C version `00` defines.
    pub flags: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

impl TraceContext {
    /// A new trace: a random trace id, a random span id, sampled.
    pub fn mint() -> TraceContext {
        TraceContext {
            trace_id: random_hex(16),
            span_id: new_span_id(),
            parent_span_id: None,
            flags: "01".into(),
            tracestate: None,
        }
    }

    /// The span a `traceparent` names, with its `tracestate`: what a carrier
    /// that agentd itself wrote reads back as (a spawn payload). `None` for a
    /// malformed `traceparent`; a `tracestate` over the limits or malformed is
    /// dropped and the `traceparent` kept.
    pub fn parse(traceparent: &str, tracestate: Option<&str>) -> Option<TraceContext> {
        let mut t = parse(traceparent)?;
        t.tracestate = tracestate.and_then(sanitize_tracestate);
        Some(t)
    }

    /// A new span continuing an upstream `traceparent`: the same trace, a span
    /// of its own whose parent is the upstream's span, the upstream's flags and
    /// `tracestate`. `None` when the `traceparent` is malformed — the caller
    /// then starts a trace of its own.
    pub fn child_of(traceparent: &str, tracestate: Option<&str>) -> Option<TraceContext> {
        Self::parse(traceparent, tracestate).map(|up| up.child())
    }

    /// Continue `incoming` when it is a valid `traceparent`, else mint.
    pub fn resolve(incoming: Option<&str>) -> TraceContext {
        incoming
            .and_then(|tp| Self::child_of(tp, None))
            .unwrap_or_else(Self::mint)
    }

    /// A child span of this one: the span every outbound hop and every child
    /// process the unit starts gets, so each has an id of its own under one
    /// parent. Flags and `tracestate` go on unchanged.
    pub fn child(&self) -> TraceContext {
        TraceContext {
            trace_id: self.trace_id.clone(),
            span_id: new_span_id(),
            parent_span_id: Some(self.span_id.clone()),
            flags: self.flags.clone(),
            tracestate: self.tracestate.clone(),
        }
    }

    /// This span as a `traceparent` (version `00`).
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-{}", self.trace_id, self.span_id, self.flags)
    }

    /// Whether the trace is sampled (flag bit 0).
    pub fn sampled(&self) -> bool {
        self.flags == "01"
    }

    /// The W3C keys for an MCP request's `params._meta` (SEP-414): this
    /// span's `traceparent`, and the `tracestate` when there is one. The
    /// caller passes the span the request is: a fresh [`Self::child`].
    pub fn meta(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("traceparent".into(), Value::String(self.traceparent()));
        if let Some(ts) = &self.tracestate {
            m.insert("tracestate".into(), Value::String(ts.clone()));
        }
        m
    }
}

/// The process's own trace: what a line or a request that belongs to no unit
/// of work carries. The supervisor sets it at start; a turn worker or a
/// subagent sets it to the unit it serves, since everything such a process
/// does is that unit's.
static PROCESS: RwLock<Option<TraceContext>> = RwLock::new(None);

/// Set the process trace.
pub fn set_process(t: TraceContext) {
    *PROCESS.write().unwrap_or_else(|e| e.into_inner()) = Some(t);
}

/// The process trace, once set.
pub fn process() -> Option<TraceContext> {
    PROCESS.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Parse a `traceparent` per W3C Trace Context §3.2: `None` for version `ff`,
/// an all-zero trace or parent id, a wrong length, a character that is not
/// lowercase hex, or anything that is not ASCII. A version above `00` is read
/// by its first four fields, and whatever follows must begin with a dash;
/// version `00` has exactly four. Only the sampled bit of the flags is kept.
///
/// The value may come from anyone who can reach agentd, so it is read as
/// bytes: a multibyte character where a field ends is refused, not sliced.
fn parse(s: &str) -> Option<TraceContext> {
    let b = s.trim().as_bytes();
    if !b.is_ascii() {
        return None;
    }
    // version "-" trace-id "-" parent-id "-" flags
    if b.len() < 55 || b[2] != b'-' || b[35] != b'-' || b[52] != b'-' {
        return None;
    }
    let (ver, tid, sid, flags) = (&b[0..2], &b[3..35], &b[36..52], &b[53..55]);
    if ![ver, tid, sid, flags].iter().all(|p| is_lower_hex(p)) || ver == b"ff" {
        return None;
    }
    match (ver, b.get(55)) {
        (b"00", None) => {}
        (b"00", Some(_)) => return None,
        (_, None | Some(b'-')) => {}
        (_, Some(_)) => return None,
    }
    if tid.iter().all(|c| *c == b'0') || sid.iter().all(|c| *c == b'0') {
        return None;
    }
    // Lowercase hex, checked above: each field is ASCII.
    let text = |f: &[u8]| String::from_utf8_lossy(f).into_owned();
    let sampled = u8::from_str_radix(&text(flags), 16).ok()? & 0x01;
    Some(TraceContext {
        trace_id: text(tid),
        span_id: text(sid),
        parent_span_id: None,
        flags: format!("{sampled:02x}"),
        tracestate: None,
    })
}

fn is_lower_hex(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A `tracestate` as agentd stores and sends it: its list-members, each a
/// valid `key=value` (§3.3.1), joined by `,` with the optional whitespace and
/// the empty members removed. `None` — dropped whole — when it is empty,
/// malformed, names a key twice, or exceeds [`TRACESTATE_MAX_MEMBERS`] members
/// or [`TRACESTATE_MAX_BYTES`] bytes: a vendor's state is passed on intact or
/// not at all.
pub fn sanitize_tracestate(s: &str) -> Option<String> {
    if s.len() > TRACESTATE_MAX_BYTES {
        return None;
    }
    let mut members: Vec<&str> = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    for m in s.split(',') {
        let m = m.trim_matches([' ', '\t']);
        if m.is_empty() {
            continue;
        }
        let (k, v) = m.split_once('=')?;
        if !valid_key(k) || !valid_value(v) || keys.contains(&k) {
            return None;
        }
        keys.push(k);
        members.push(m);
    }
    if members.is_empty() || members.len() > TRACESTATE_MAX_MEMBERS {
        return None;
    }
    Some(members.join(","))
}

/// `simple-key` or `tenant-id@system-id` (§3.3.1.3.1).
fn valid_key(k: &str) -> bool {
    let rest = |s: &str| {
        s.bytes()
            .all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'*' | b'/'))
    };
    match k.split_once('@') {
        None => {
            k.len() <= 256 && k.as_bytes().first().is_some_and(u8::is_ascii_lowercase) && rest(k)
        }
        Some((tenant, system)) => {
            tenant.len() <= 241
                && tenant
                    .as_bytes()
                    .first()
                    .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                && rest(tenant)
                && system.len() <= 14
                && system
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_lowercase)
                && rest(system)
        }
    }
}

/// Up to 256 printable ASCII characters other than `,` and `=`, the last not
/// a space (§3.3.1.3.2).
fn valid_value(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 256
        && !v.ends_with(' ')
        && v.bytes()
            .all(|c| matches!(c, 0x20..=0x7e) && c != b',' && c != b'=')
}

/// `n` random bytes as lowercase hex, never all zeros. Read from the kernel's
/// CSPRNG; a host without one — which a daemon refuses to start on — gets ids
/// mixed from the clock, the pid and a counter, unique if not unpredictable.
fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    if crate::sec::random::fill(&mut buf).is_err() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut z = nanos
            ^ (std::process::id() as u64).rotate_left(32)
            ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
        for chunk in buf.chunks_mut(8) {
            z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            x ^= x >> 31;
            chunk.copy_from_slice(&x.to_le_bytes()[..chunk.len()]);
        }
    }
    if buf.iter().all(|b| *b == 0) {
        buf[n - 1] = 1;
    }
    crate::sha::to_hex(&buf)
}

/// A fresh random 8-byte (16-hex) span id. Public so the `otel` span export
/// mints its span ids the same way.
pub fn new_span_id() -> String {
    random_hex(8)
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn parse_valid_traceparent() {
        let tc = TraceContext::parse(UP, None).unwrap();
        assert_eq!(tc.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tc.span_id, "00f067aa0ba902b7");
        assert_eq!(tc.flags, "01");
        assert_eq!(tc.traceparent(), UP);
    }

    #[test]
    fn parse_rejects_malformed() {
        for bad in [
            "garbage",
            "00-short-00f067aa0ba902b7-01",
            "00-zzzz2f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // Uppercase hex is not the W3C alphabet.
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            // All-zero ids are invalid.
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            // Version ff is forbidden.
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            // Version 00 has exactly four fields.
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            // Wrong lengths.
            "00-4bf92f3577b34da6a3ce929d0e0e473-600f067aa0ba902b7-01",
            "0-04bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
            // A future version still needs a dash after the flags.
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01x",
            // A multibyte character where a field ends is refused, not sliced
            // through: the flags' second byte, in either version, and what
            // follows a future version's flags.
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0é",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-0é",
            "00-4bf92f3577b34da6a3ce929d0e0e473é-00f067aa0ba902b7-01",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-é",
        ] {
            assert!(TraceContext::parse(bad, None).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_future_version_is_read_by_its_first_four_fields() {
        let tc = TraceContext::parse(
            "cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-what-the-future-holds",
            None,
        )
        .unwrap();
        assert_eq!(tc.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tc.span_id, "00f067aa0ba902b7");
        // agentd answers in the version it speaks.
        assert!(tc.traceparent().starts_with("00-"));
        let tc = TraceContext::parse(
            "cc-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-09",
            None,
        )
        .unwrap();
        // Only the sampled bit is kept.
        assert_eq!(tc.flags, "01");
    }

    #[test]
    fn continuing_keeps_the_trace_and_parents_on_the_upstream_span() {
        let tc = TraceContext::child_of(UP, Some("congo=t61rcWkgMzE")).unwrap();
        assert_eq!(tc.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_ne!(tc.span_id, "00f067aa0ba902b7");
        assert_eq!(tc.parent_span_id.as_deref(), Some("00f067aa0ba902b7"));
        assert_eq!(tc.flags, "01");
        assert_eq!(tc.tracestate.as_deref(), Some("congo=t61rcWkgMzE"));
    }

    /// An upstream that did not sample stays unsampled on every hop below it:
    /// the decision is the upstream's, not agentd's to overturn.
    #[test]
    fn an_unsampled_upstream_stays_unsampled_downstream() {
        let up = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";
        let unit = TraceContext::resolve(Some(up));
        assert_eq!(unit.flags, "00");
        assert!(!unit.sampled());
        let hop = unit.child().child();
        assert!(hop.traceparent().ends_with("-00"), "{}", hop.traceparent());
        assert_eq!(hop.trace_id, unit.trace_id);
    }

    #[test]
    fn a_child_span_keeps_trace_flags_and_state() {
        let unit = TraceContext::child_of(UP, Some("rojo=00f067aa0ba902b7")).unwrap();
        let a = unit.child();
        let b = unit.child();
        assert_eq!(a.trace_id, unit.trace_id);
        assert_eq!(a.parent_span_id.as_deref(), Some(unit.span_id.as_str()));
        assert_ne!(a.span_id, b.span_id);
        assert_eq!(a.tracestate, unit.tracestate);
        let meta = a.meta();
        assert_eq!(meta["traceparent"], a.traceparent());
        assert_eq!(meta["tracestate"], "rojo=00f067aa0ba902b7");
    }

    #[test]
    fn minted_traces_are_random_and_sampled() {
        let a = TraceContext::mint();
        let b = TraceContext::mint();
        assert_ne!(a.trace_id, b.trace_id);
        assert_eq!(a.trace_id.len(), 32);
        assert_eq!(a.span_id.len(), 16);
        assert!(is_lower_hex(a.trace_id.as_bytes()) && is_lower_hex(a.span_id.as_bytes()));
        assert_eq!(a.flags, "01");
        assert!(a.parent_span_id.is_none());
        assert!(TraceContext::parse(&a.traceparent(), None).is_some());
    }

    /// Nothing about a process's trace follows from its run id: a pinned
    /// `--run-id` used to give every life of a daemon the same trace.
    #[test]
    fn resolve_mints_a_new_trace_on_a_missing_or_invalid_incoming() {
        let a = TraceContext::resolve(None);
        let b = TraceContext::resolve(None);
        assert_ne!(a.trace_id, b.trace_id);
        let c = TraceContext::resolve(Some("not-a-traceparent"));
        assert_eq!(c.trace_id.len(), 32);
        assert!(c.parent_span_id.is_none());
        assert_eq!(c.flags, "01");
    }

    #[test]
    fn tracestate_is_kept_within_the_limits_and_dropped_beyond_them() {
        assert_eq!(
            sanitize_tracestate(" rojo=00f067aa0ba902b7 ,, congo=t61rcWkgMzE\t").as_deref(),
            Some("rojo=00f067aa0ba902b7,congo=t61rcWkgMzE")
        );
        assert_eq!(
            sanitize_tracestate("fw529a3039@dt=r1,1tenant@vendor=x").as_deref(),
            Some("fw529a3039@dt=r1,1tenant@vendor=x")
        );
        // 32 members are fine; 33 are dropped whole.
        let members = |n: usize| -> String {
            (0..n)
                .map(|i| format!("k{i}=v"))
                .collect::<Vec<_>>()
                .join(",")
        };
        assert!(sanitize_tracestate(&members(32)).is_some());
        assert!(sanitize_tracestate(&members(33)).is_none());
        // Over 512 bytes is dropped, never cut.
        let long = format!("a={}", "x".repeat(255)) + &format!(",b={}", "y".repeat(250));
        assert!(long.len() <= 512 && sanitize_tracestate(&long).is_some());
        let longer = long + ",c=zzzz";
        assert!(longer.len() > 512);
        assert!(sanitize_tracestate(&longer).is_none());
        for bad in [
            "",
            " , ",
            "noequals",
            "Upper=x",
            "k=v,k=w",
            "k=has,comma",
            "k=a=b",
            "k=",
            "-k=v",
        ] {
            assert!(sanitize_tracestate(bad).is_none(), "{bad:?}");
        }
    }

    /// A malformed or over-limit `tracestate` costs only itself: the
    /// `traceparent` beside it is still continued.
    #[test]
    fn a_bad_tracestate_is_dropped_and_the_traceparent_kept() {
        let tc = TraceContext::child_of(UP, Some(&"k=v,".repeat(40))).unwrap();
        assert_eq!(tc.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert!(tc.tracestate.is_none());
        assert!(!tc.meta().contains_key("tracestate"));
    }

    #[test]
    fn a_trace_survives_its_record() {
        let tc = TraceContext::child_of(UP, Some("congo=t61rcWkgMzE")).unwrap();
        let v = serde_json::to_value(&tc).unwrap();
        assert_eq!(serde_json::from_value::<TraceContext>(v).unwrap(), tc);
    }
}

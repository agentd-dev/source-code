// SPDX-License-Identifier: Apache-2.0
//! W3C Trace Context propagation. RFC 0010 §context-propagation.
//!
//! **Propagation is default-on and dependency-free** — a `trace_id` shared
//! across the whole agent tree (ingested from an upstream `traceparent` or
//! minted from the run id), threaded into every process's log lines, the spawn
//! payload, and outbound MCP `_meta`. So a single run — supervisor + every
//! subagent + every tool call — is one correlatable, auditable trace, with no
//! collector required. Span *export* (OTLP) is the only otel-gated part.
//!
//! `traceparent` = `00-<32-hex trace-id>-<16-hex span-id>-<2-hex flags>`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// A W3C trace context for one process: the shared trace id, this process's
/// span id, and the sampling flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: String,
    pub span_id: String,
    pub flags: String,
}

impl TraceContext {
    /// The `traceparent` header/`_meta` value to emit on outbound calls.
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-{}", self.trace_id, self.span_id, self.flags)
    }
}

/// An outbound `traceparent` for a given trace id with a fresh child span —
/// for stamping onto MCP `_meta` / the LLM header so downstream services see
/// this agent as their parent (W3C-correct). RFC 0010.
pub fn outbound_traceparent(trace_id: &str) -> String {
    format!("00-{}-{}-01", trace_id, new_span_id())
}

/// Resolve the trace context for a run: continue an upstream `traceparent` if a
/// valid one is supplied (same trace id, fresh span), else mint a fresh trace
/// deterministically from the run id (so retries of a run share a trace id).
pub fn resolve(run_id: &str, incoming: Option<&str>) -> TraceContext {
    match incoming.and_then(parse) {
        Some(up) => TraceContext {
            trace_id: up.trace_id,
            span_id: new_span_id(),
            flags: up.flags,
        },
        None => TraceContext {
            trace_id: trace_id_from(run_id),
            span_id: new_span_id(),
            flags: "01".into(),
        },
    }
}

/// Parse a `traceparent`. Returns `None` for any malformed or all-zero
/// (invalid per spec) value.
pub fn parse(s: &str) -> Option<TraceContext> {
    let p: Vec<&str> = s.trim().split('-').collect();
    if p.len() != 4 {
        return None;
    }
    let (ver, tid, sid, flags) = (p[0], p[1], p[2], p[3]);
    if ver.len() != 2 || tid.len() != 32 || sid.len() != 16 || flags.len() != 2 {
        return None;
    }
    if ![ver, tid, sid, flags].iter().all(|s| is_hex(s)) {
        return None;
    }
    if tid.bytes().all(|b| b == b'0') || sid.bytes().all(|b| b == b'0') {
        return None; // all-zero ids are invalid
    }
    Some(TraceContext {
        trace_id: tid.to_string(),
        span_id: sid.to_string(),
        flags: flags.to_string(),
    })
}

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// FNV-1a 64-bit — a tiny non-crypto hash (a trace/span id needs uniqueness,
/// not unpredictability; this keeps the dependency budget at zero).
fn fnv1a(data: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A 16-byte (32-hex) trace id derived deterministically from the run id, so
/// the same `--run-id` always maps to the same trace.
fn trace_id_from(run_id: &str) -> String {
    let a = fnv1a(run_id.as_bytes(), 0xcbf2_9ce4_8422_2325) | 1; // avoid all-zero
    let b = fnv1a(run_id.as_bytes(), 0x8422_2325_cbf2_9ce4);
    format!("{a:016x}{b:016x}")
}

/// A fresh 8-byte (16-hex) span id, unique per call (time ⊕ pid ⊕ counter).
/// Public so the `otel` span export can mint child span ids under the run trace.
pub fn new_span_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = 0xcbf2_9ce4_8422_2325 ^ (std::process::id() as u64) ^ n;
    let mixed = fnv1a(&nanos.to_le_bytes(), seed) | 1; // avoid all-zero
    format!("{mixed:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_traceparent() {
        let tc = parse("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(tc.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tc.span_id, "00f067aa0ba902b7");
        assert_eq!(tc.flags, "01");
        assert_eq!(
            tc.traceparent(),
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse("garbage").is_none());
        assert!(parse("00-short-00f067aa0ba902b7-01").is_none());
        assert!(parse("00-zzzz2f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").is_none());
        // all-zero trace id is invalid
        assert!(parse("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none());
    }

    #[test]
    fn resolve_continues_upstream_trace() {
        let up = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let tc = resolve("run-1", Some(up));
        assert_eq!(tc.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736"); // same trace
        assert_ne!(tc.span_id, "00f067aa0ba902b7"); // fresh span
        assert_eq!(tc.flags, "01");
    }

    #[test]
    fn resolve_mints_deterministically_from_run_id() {
        let a = resolve("run-abc", None);
        let b = resolve("run-abc", None);
        assert_eq!(a.trace_id, b.trace_id); // same run id → same trace id
        assert_ne!(a.span_id, b.span_id); // but distinct spans
        assert_eq!(a.trace_id.len(), 32);
        let c = resolve("run-xyz", None);
        assert_ne!(a.trace_id, c.trace_id); // different run id → different trace
    }

    #[test]
    fn resolve_mints_on_invalid_incoming() {
        let tc = resolve("run-1", Some("not-a-traceparent"));
        assert_eq!(tc.trace_id.len(), 32);
        assert_eq!(tc.flags, "01");
    }
}

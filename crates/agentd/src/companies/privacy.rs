// SPDX-License-Identifier: AGPL-3.0-only
//! The strict privacy profile's own values (RFC 0047 §5.10): what a Company
//! call carries in place of agentd's correlators, so a User cannot be
//! matched across Companies by them (draft §4.2: "Companies can't match
//! Users with each other").
//!
//! Each is derived with HMAC-SHA256 under the random secret of the (User,
//! Company) pair, kept with the User ID ([`super::user_id::UserRecord`]) and
//! never sent:
//!
//! - **the idempotency key** — the call's own key, keyed: a retry of the
//!   same call still carries the same key, and two Companies never see one
//!   key alike;
//! - **the trace** — a trace of its own per (unit of work, Company): every
//!   call one unit makes to one Company is in one trace there, and two
//!   Companies the unit calls see two traces, neither of them the unit's.
//!   The span agentd records for the call stays in the unit's trace, and
//!   links to this one, so an operator can still follow it.
//!
//! A connection's own requests — the handshake, the tool list — carry no
//! trace context at all under the profile: the supervisor holds every
//! Company connection, so the process trace would be one id at every
//! Company.

use crate::obs::trace::{TraceContext, new_span_id};

fn keyed(secret: &str, label: &str, value: &str) -> [u8; 32] {
    crate::sha::hmac_sha256(secret.as_bytes(), format!("{label}|{value}").as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `agent/idempotency_key` a Company is sent for a call whose own key is
/// `key`.
pub fn idempotency_key(secret: &str, key: &str) -> String {
    hex(&keyed(secret, "idempotency", key))
}

/// The span a Company is sent for one call of the unit of work `unit`: the
/// trace the (unit, Company) pair has there, a span of its own with no
/// parent, the unit's sampling flag and no `tracestate` — a vendor's state
/// would be the same at every Company.
pub fn reroot(secret: &str, unit: &TraceContext) -> TraceContext {
    TraceContext {
        trace_id: hex(&keyed(secret, "trace", &unit.trace_id)[..16]),
        span_id: new_span_id(),
        parent_span_id: None,
        flags: unit.flags.clone(),
        tracestate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One pair's key is stable for one call and differs for another; two
    /// pairs never share one.
    #[test]
    fn a_key_is_the_pairs_own_and_stable_across_retries() {
        let (a, b) = ("secret-a", "secret-b");
        let k = idempotency_key(a, "inst/step:r1/s#1");
        assert_eq!(k, idempotency_key(a, "inst/step:r1/s#1"), "a retry");
        assert_ne!(k, idempotency_key(a, "inst/step:r1/s#2"));
        assert_ne!(k, idempotency_key(b, "inst/step:r1/s#1"), "another Company");
        assert_eq!(k.len(), 64);
        assert!(!k.contains("inst"), "the key itself is not in it: {k}");
    }

    /// A unit's calls to one Company share a trace there; another Company
    /// sees another trace; neither is the unit's, and none has a parent or a
    /// `tracestate`.
    #[test]
    fn each_company_sees_a_trace_of_its_own() {
        let mut unit = TraceContext::mint();
        unit.tracestate = Some("vendor=abc".into());
        let one = reroot("secret-a", &unit);
        let again = reroot("secret-a", &unit);
        let other = reroot("secret-b", &unit);
        assert_eq!(
            one.trace_id, again.trace_id,
            "one trace per (unit, Company)"
        );
        assert_ne!(one.span_id, again.span_id, "a span per call");
        assert_ne!(one.trace_id, other.trace_id);
        assert_ne!(one.trace_id, unit.trace_id);
        assert_ne!(other.trace_id, unit.trace_id);
        assert_eq!(one.trace_id.len(), 32);
        assert!(one.parent_span_id.is_none() && one.tracestate.is_none());
        assert_eq!(one.flags, unit.flags);
        assert!(TraceContext::parse(&one.traceparent(), None).is_some());
        let next = TraceContext::mint();
        assert_ne!(
            reroot("secret-a", &next).trace_id,
            one.trace_id,
            "another unit"
        );
    }
}

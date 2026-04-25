//! W3C Trace Context `traceparent` header parsing.
//!
//! <https://www.w3.org/TR/trace-context/#traceparent-header>
//!
//! Format: `VERSION-TRACE_ID-PARENT_ID-TRACE_FLAGS`, where:
//! - `VERSION` is a 2-hex-digit byte. Only `00` is defined today;
//!   other versions are accepted so we pass through forward-compat
//!   headers without a hard fail.
//! - `TRACE_ID` is a 32-hex-digit (128-bit) value, non-zero.
//! - `PARENT_ID` is a 16-hex-digit (64-bit) value, non-zero.
//! - `TRACE_FLAGS` is a 2-hex-digit byte; bit 0 is the `sampled` flag.
//!
//! We don't depend on the `opentelemetry` crate — spec parsing is
//! a handful of lines and keeps the dep tree small (§10 maturity
//! doc on staying dep-light). Emitted fields flow into the tracing
//! subscriber as structured values so the JSON log stream carries
//! them verbatim into any OTLP collector's filelog receiver (which
//! is the recommended integration path until a direct OTLP exporter
//! ships).

/// Parsed `traceparent` header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceParent {
    pub version: String,
    pub trace_id: String,
    pub parent_id: String,
    pub trace_flags: String,
}

impl TraceParent {
    /// `sampled` bit (low bit of `trace_flags`) — scrapers can use
    /// this to drop unsampled traces at the receiver.
    pub fn sampled(&self) -> bool {
        u8::from_str_radix(&self.trace_flags, 16)
            .map(|b| b & 0x01 != 0)
            .unwrap_or(false)
    }

    /// Render back to the canonical W3C `traceparent` header value
    /// (`VERSION-TRACE_ID-PARENT_ID-FLAGS`). Used by outbound
    /// tooling (the `http_request` handler) to continue a received
    /// trace across an agent → downstream call.
    pub fn format(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.version, self.trace_id, self.parent_id, self.trace_flags
        )
    }

    /// Produce an outbound header that keeps the inbound trace-id +
    /// flags but swaps the parent-id to a freshly-generated span id
    /// representing the agent. Downstream services then see the
    /// agent as their direct parent rather than whoever called
    /// the agent — which is what W3C intends.
    pub fn with_parent_id(&self, new_parent_id: &str) -> Self {
        Self {
            version: self.version.clone(),
            trace_id: self.trace_id.clone(),
            parent_id: new_parent_id.to_string(),
            trace_flags: self.trace_flags.clone(),
        }
    }
}

/// Generate a fresh 16-hex (8-byte) span id suitable for the
/// `parent_id` slot of an outbound `traceparent`. Uses a
/// thread-local RNG seeded from the system clock — good enough for
/// span identifiers (spec requires randomness, not unpredictability).
/// Never returns all-zeros (spec-invalid).
pub fn fresh_span_id() -> String {
    use std::cell::Cell;
    use std::time::SystemTime;
    thread_local! {
        static COUNTER: Cell<u64> = const { Cell::new(0) };
    }
    let c = COUNTER.with(|c| {
        let next = c.get().wrapping_add(1);
        c.set(next);
        next
    });
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Mix counter into nanos so rapid successive calls produce
    // distinct ids even when the clock resolution coarsens.
    let mut id = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(c);
    if id == 0 {
        id = 1;
    }
    format!("{id:016x}")
}

/// Parse the raw `traceparent` header value. Returns `None` if the
/// header is malformed, the trace-id or parent-id are the all-zeros
/// sentinel, or any segment is the wrong length.
///
/// Lenient on version: we accept any 2-hex value so a future-dated
/// `01-...` trace context still gets its IDs through to the logs.
pub fn parse_traceparent(raw: &str) -> Option<TraceParent> {
    let parts: Vec<&str> = raw.trim().split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    let (version, trace_id, parent_id, flags) = (parts[0], parts[1], parts[2], parts[3]);

    if version.len() != 2 || !is_hex(version) {
        return None;
    }
    if trace_id.len() != 32 || !is_hex(trace_id) || trace_id.bytes().all(|b| b == b'0') {
        return None;
    }
    if parent_id.len() != 16 || !is_hex(parent_id) || parent_id.bytes().all(|b| b == b'0') {
        return None;
    }
    if flags.len() != 2 || !is_hex(flags) {
        return None;
    }

    Some(TraceParent {
        version: version.to_ascii_lowercase(),
        trace_id: trace_id.to_ascii_lowercase(),
        parent_id: parent_id.to_ascii_lowercase(),
        trace_flags: flags.to_ascii_lowercase(),
    })
}

fn is_hex(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_header() {
        let tp =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01").unwrap();
        assert_eq!(tp.version, "00");
        assert_eq!(tp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tp.parent_id, "00f067aa0ba902b7");
        assert_eq!(tp.trace_flags, "01");
        assert!(tp.sampled());
    }

    #[test]
    fn unsampled_flag_zero() {
        let tp =
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00").unwrap();
        assert!(!tp.sampled());
    }

    #[test]
    fn normalizes_to_lowercase() {
        let tp =
            parse_traceparent("00-4BF92F3577B34DA6A3CE929D0E0E4736-00F067AA0BA902B7-01").unwrap();
        assert_eq!(tp.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(tp.parent_id, "00f067aa0ba902b7");
    }

    #[test]
    fn accepts_future_version() {
        // Spec says unknown versions should still have their fields
        // respected when the layout matches. Forward-compat.
        let tp = parse_traceparent("ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        assert!(tp.is_some());
    }

    #[test]
    fn rejects_all_zero_trace_id() {
        assert!(
            parse_traceparent("00-00000000000000000000000000000000-00f067aa0ba902b7-01").is_none()
        );
    }

    #[test]
    fn rejects_all_zero_parent_id() {
        assert!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01").is_none()
        );
    }

    #[test]
    fn rejects_non_hex() {
        assert!(
            parse_traceparent("00-zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-00f067aa0ba902b7-01").is_none()
        );
    }

    #[test]
    fn rejects_wrong_segment_count() {
        assert!(
            parse_traceparent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7").is_none()
        );
        assert!(parse_traceparent("no-dashes-at-all-ha").is_none());
        assert!(parse_traceparent("").is_none());
    }

    #[test]
    fn rejects_wrong_trace_id_length() {
        assert!(parse_traceparent("00-4bf92f3577b34da6-00f067aa0ba902b7-01").is_none());
    }
}

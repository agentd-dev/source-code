//! The failover decision — sticky-primary with bounded sweep. RFC 0018 §3.3.
//!
//! The client's `complete()` is wrapped: try the **active** endpoint; on a
//! FAILOVER-CLASS error (connect refused/reset, timeout, HTTP 5xx, 429 after the
//! endpoint's retry, or a circuit-open skip) advance to the next *available*
//! endpoint in list order; a *non*-failover error (401/403 auth, 4xx request,
//! malformed body) is returned immediately (it is the same on every endpoint).
//! On success, snap `active` back to the lowest-index healthy endpoint
//! (sticky-primary) so a fallback is temporary by construction.
//!
//! §3.4: the wire/adapter/JSON path is UNCHANGED — this is the only net-new
//! control flow. Each `complete_once` still dials fresh (RFC 0006 §7); the only
//! state kept between calls is the cheap per-endpoint health/breaker record.

use std::time::Duration;

use super::client::IntelError;
use super::endpoints::EndpointList;
use super::health::{BreakerTransition, ErrKind};
use crate::wire::intel::{Request, Response};

/// How a single endpoint's outcome is classified for failover (RFC 0018 §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverClass {
    /// Try the next endpoint (connect refused/reset, timeout, 5xx, 429).
    Failover(ErrKind),
    /// Do not fail over — fatal/observation, identical on every endpoint
    /// (auth 401/403, 4xx, malformed body).
    Fatal,
}

/// Classify an [`IntelError`] for the failover sweep (RFC 0018 §3.3 table). This
/// EXTENDS RFC 0006 §3 / RFC 0007 §3.6 — it does not redefine the per-call retry,
/// which has already run inside `complete_once` before we get here.
pub fn classify(err: &IntelError) -> FailoverClass {
    match err {
        // Transport-layer failures are always failover-class: the endpoint is
        // down/moving/wedged — a sibling may be fine.
        IntelError::Transport(e) => {
            use std::io::ErrorKind::*;
            let kind = match e.kind() {
                ConnectionRefused => ErrKind::Refused,
                ConnectionReset | ConnectionAborted | BrokenPipe => ErrKind::Reset,
                TimedOut | WouldBlock => ErrKind::Timeout,
                _ => ErrKind::Refused, // NotFound (DNS), other I/O → treat as down
            };
            FailoverClass::Failover(kind)
        }
        // HTTP status: 5xx and 429 are failover-class; 401/403 (auth) and other
        // 4xx are fatal — a bad request/credential is bad on every endpoint.
        IntelError::Http(code, _) => match *code {
            500 | 502 | 503 | 504 => FailoverClass::Failover(ErrKind::Http5xx),
            429 => FailoverClass::Failover(ErrKind::Http429),
            // any other 5xx is still upstream-transient
            c if (500..600).contains(&c) => FailoverClass::Failover(ErrKind::Http5xx),
            _ => FailoverClass::Fatal, // 401/403/4xx
        },
        // A malformed body is a bad response everywhere → observation/abort.
        IntelError::Parse(_) => FailoverClass::Fatal,
        // An unsupported transport is a config error, not a transient outage.
        IntelError::Unsupported(_) => FailoverClass::Fatal,
        // All-endpoints-down is already terminal — not re-classified.
        IntelError::AllEndpointsDown(_) => FailoverClass::Fatal,
    }
}

/// Is this a fatal **auth** failure (401/403)? §6 distinguishes it from all-down:
/// an auth failure on every endpoint is a misconfig (exit 4 immediately), NOT a
/// backoff-loop — masking a credential error as a transient outage would hide it.
pub fn is_auth(err: &IntelError) -> bool {
    matches!(err, IntelError::Http(401 | 403, _))
}

/// The result of one failover sweep, plus the side-channel of breaker/active
/// transitions the caller surfaces as metrics/events (§4.3/§8) and the
/// `agentd://intelligence` emission (§4.4).
pub struct SweepResult {
    pub outcome: Result<Response, IntelError>,
    /// `(from, to)` if a failover advanced the endpoint within the sweep.
    pub failover: Option<(usize, usize)>,
    /// Breaker transitions observed, as `(endpoint_index, transition)`.
    pub breaker_changes: Vec<(usize, BreakerTransition)>,
    /// The new active index if it changed (failover or snap-back).
    pub active_change: Option<usize>,
    /// The endpoint that ultimately served the request (on success).
    pub served_by: Option<usize>,
}

/// Drive one bounded failover sweep for a single logical `complete` (RFC 0018
/// §3.3). Visits at most `eps.len()` distinct endpoints (each at most once).
pub fn complete_resilient(
    list: &mut EndpointList,
    req: &Request,
    timeout: Duration,
    trace_id: Option<&str>,
) -> SweepResult {
    let order = list.attempt_order();
    let cfg = *list.breaker_config();
    let mut breaker_changes = Vec::new();
    let mut failover = None;
    let mut last_err: Option<IntelError> = None;
    let mut prev_idx: Option<usize> = None;

    // All endpoints down (every breaker OPEN-and-cooling): the documented §6
    // terminal — surfaced by the caller as exit-4 (once) / backoff (loop).
    if order.is_empty() {
        return SweepResult {
            outcome: Err(IntelError::AllEndpointsDown(None)),
            failover: None,
            breaker_changes,
            active_change: None,
            served_by: None,
        };
    }

    for idx in order {
        // A second-or-later attempt in the sweep IS a failover advance (§8 event).
        if let Some(prev) = prev_idx
            && prev != idx
        {
            failover = Some((prev, idx));
        }
        prev_idx = Some(idx);

        match list.ep(idx).complete_once(req, timeout, trace_id) {
            Ok((resp, latency)) => {
                if let Some(t) = list.ep(idx).health.record_success(latency) {
                    breaker_changes.push((idx, t));
                }
                let mut active_change = list.set_active(idx);
                // Snap back to the lowest-index healthy endpoint (sticky-primary).
                if let Some(snapped) = list.prefer_lowest_healthy() {
                    active_change = Some(snapped);
                }
                return SweepResult {
                    outcome: Ok(resp),
                    failover,
                    breaker_changes,
                    active_change,
                    served_by: Some(idx),
                };
            }
            Err(e) => match classify(&e) {
                FailoverClass::Failover(kind) => {
                    if let Some(t) = list.ep(idx).health.record_failure(kind, &cfg) {
                        breaker_changes.push((idx, t));
                    }
                    last_err = Some(e);
                    continue; // advance to the next available endpoint
                }
                FailoverClass::Fatal => {
                    // Auth/4xx/malformed: same on every endpoint → return now.
                    return SweepResult {
                        outcome: Err(e),
                        failover,
                        breaker_changes,
                        active_change: None,
                        served_by: None,
                    };
                }
            },
        }
    }

    // Every available endpoint failed over → all down (§6).
    SweepResult {
        outcome: Err(IntelError::AllEndpointsDown(last_err.map(Box::new))),
        failover,
        breaker_changes,
        active_change: None,
        served_by: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn io_err(kind: io::ErrorKind) -> IntelError {
        IntelError::Transport(io::Error::new(kind, "x"))
    }

    #[test]
    fn transport_errors_are_failover_class() {
        assert!(matches!(
            classify(&io_err(io::ErrorKind::ConnectionRefused)),
            FailoverClass::Failover(ErrKind::Refused)
        ));
        assert!(matches!(
            classify(&io_err(io::ErrorKind::TimedOut)),
            FailoverClass::Failover(ErrKind::Timeout)
        ));
        assert!(matches!(
            classify(&io_err(io::ErrorKind::ConnectionReset)),
            FailoverClass::Failover(ErrKind::Reset)
        ));
    }

    #[test]
    fn http_5xx_and_429_failover_but_4xx_does_not() {
        assert!(matches!(
            classify(&IntelError::Http(503, "x".into())),
            FailoverClass::Failover(ErrKind::Http5xx)
        ));
        assert!(matches!(
            classify(&IntelError::Http(429, "x".into())),
            FailoverClass::Failover(ErrKind::Http429)
        ));
        // auth / request error → fatal, NOT failover
        assert_eq!(
            classify(&IntelError::Http(401, "x".into())),
            FailoverClass::Fatal
        );
        assert_eq!(
            classify(&IntelError::Http(403, "x".into())),
            FailoverClass::Fatal
        );
        assert_eq!(
            classify(&IntelError::Http(400, "x".into())),
            FailoverClass::Fatal
        );
        assert_eq!(
            classify(&IntelError::Http(404, "x".into())),
            FailoverClass::Fatal
        );
    }

    #[test]
    fn malformed_body_is_fatal_not_failover() {
        assert_eq!(
            classify(&IntelError::Parse("bad json".into())),
            FailoverClass::Fatal
        );
    }

    #[test]
    fn auth_detection_distinguishes_from_all_down() {
        assert!(is_auth(&IntelError::Http(401, "x".into())));
        assert!(is_auth(&IntelError::Http(403, "x".into())));
        assert!(!is_auth(&IntelError::Http(503, "x".into())));
        assert!(!is_auth(&io_err(io::ErrorKind::ConnectionRefused)));
    }

    // --- Sweep integration tests over real TCP endpoints -------------------
    // A tiny single-shot HTTP server returns a fixed status (+ a canned
    // OpenAI-compatible body for 200) so the sweep dials a *real* endpoint via
    // `complete_once`. A closed/never-bound port gives a connect failure.

    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Bind `127.0.0.1:0`, serve one request returning `status`, and return the
    /// `http://127.0.0.1:<port>` URI. The thread self-terminates after one conn.
    fn serve_status(status: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf); // drain the request
                let body = if status == 200 {
                    r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#
                } else {
                    r#"{"error":{"message":"boom"}}"#
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// A bound-then-dropped listener yields a port nothing listens on → connect
    /// refused (a failover-class transport error).
    fn dead_endpoint() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    fn req() -> Request {
        Request {
            model: "m".into(),
            messages: vec![crate::wire::intel::Message::user("hi")],
            tools: Vec::new(),
            max_tokens: 16,
            temperature: Some(0.0),
        }
    }

    fn list_of(uris: &[String]) -> EndpointList {
        EndpointList::parse_with_env(&uris.join(","), None, &|_| None).unwrap()
    }

    #[test]
    fn connect_failure_advances_to_next_healthy_endpoint() {
        let good = serve_status(200);
        let mut list = list_of(&[dead_endpoint(), good]);
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(r.outcome.is_ok(), "sweep failed over to the healthy ep");
        assert_eq!(r.served_by, Some(1));
        // a failover advance was recorded (0 → 1)
        assert_eq!(r.failover, Some((0, 1)));
    }

    #[test]
    fn http_5xx_advances_to_next_endpoint() {
        let bad = serve_status(503);
        let good = serve_status(200);
        let mut list = list_of(&[bad, good]);
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(r.outcome.is_ok());
        assert_eq!(r.served_by, Some(1));
    }

    #[test]
    fn http_4xx_does_not_failover() {
        let bad = serve_status(400);
        let good = serve_status(200);
        let mut list = list_of(&[bad, good]);
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        // a 4xx is fatal — the sweep returns it WITHOUT trying endpoint 1.
        assert!(matches!(r.outcome, Err(IntelError::Http(400, _))));
        assert_eq!(r.served_by, None);
        assert_eq!(r.failover, None);
    }

    #[test]
    fn auth_401_does_not_failover() {
        let bad = serve_status(401);
        let good = serve_status(200);
        let mut list = list_of(&[bad, good]);
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(matches!(r.outcome, Err(IntelError::Http(401, _))));
        assert!(is_auth(&r.outcome.unwrap_err()));
    }

    #[test]
    fn circuit_broken_endpoint_is_skipped() {
        let good = serve_status(200);
        let mut list = list_of(&[dead_endpoint(), good]);
        let cfg = *list.breaker_config();
        // open endpoint 0's breaker up front (threshold 3)
        for _ in 0..3 {
            list.ep(0).health.record_failure(ErrKind::Refused, &cfg);
        }
        // the sweep skips the broken endpoint 0 entirely → serves on 1, no
        // failover advance recorded (0 was never dialed).
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(r.outcome.is_ok());
        assert_eq!(r.served_by, Some(1));
        assert_eq!(r.failover, None, "broken ep was skipped, not failed-over");
    }

    #[test]
    fn all_endpoints_down_yields_all_endpoints_down_error() {
        let mut list = list_of(&[dead_endpoint(), dead_endpoint()]);
        // One sweep over two dead endpoints exhausts the list (each failed-over)
        // → the documented §6 terminal, mapped to exit 4 on `once`.
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(matches!(r.outcome, Err(IntelError::AllEndpointsDown(_))));
        // After enough sweeps the breakers open and `all_down()` (breaker-state)
        // also reports true — at which point `attempt_order()` is empty.
        for _ in 0..3 {
            let _ = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        }
        assert!(list.all_down());
        assert!(list.attempt_order().is_empty());
    }
}

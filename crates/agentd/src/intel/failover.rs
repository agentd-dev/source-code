// SPDX-License-Identifier: AGPL-3.0-only
//! The failover decision — sticky-primary with a bounded sweep.
//!
//! The client's `complete()` is wrapped: try the **active** endpoint; on a
//! FAILOVER-CLASS error (connect refused or reset, timeout, HTTP 5xx, 429 that
//! survived the endpoint's own retry, or a circuit-open skip) advance to the
//! next *available* endpoint in list order. A *non*-failover error — 401/403
//! auth, a 4xx request error, a malformed body — returns immediately, because
//! it would be identical on every endpoint and trying the rest would only burn
//! the run deadline while hiding the real cause. On success, `active` snaps back
//! to the lowest-index healthy endpoint, so serving from a fallback is temporary
//! by construction.
//!
//! This module holds the entire selection control flow; the wire, adapter and
//! JSON path sit below it untouched. Each `complete_once` dials a fresh
//! connection, which is what makes a re-dial safe to attempt at all. The only
//! state kept between calls is the cheap per-endpoint health and breaker record.

use std::time::Duration;

use super::client::IntelError;
use super::endpoints::EndpointList;
use super::health::{BreakerTransition, ErrKind};
use crate::wire::intel::{Request, Response};

/// How a single endpoint's outcome is classified for failover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverClass {
    /// Try the next endpoint (connect refused/reset, timeout, 5xx, 429).
    Failover(ErrKind),
    /// Do not fail over — fatal/observation, identical on every endpoint
    /// (auth 401/403, 4xx, malformed body).
    Fatal,
}

/// Classify an [`IntelError`] for the failover sweep. This decides only whether
/// to try ANOTHER endpoint; the same-endpoint transient retry has already run
/// inside `complete_once` before an error reaches here, so anything classified
/// `Failover` has already survived that retry.
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

/// Is this a fatal **auth** failure (401/403)? The all-down backoff needs to
/// distinguish the two: an auth failure on every endpoint is a misconfiguration
/// and exits 4 immediately rather than entering the backoff loop. Retrying a
/// credential error would mask it as a transient outage and leave the operator
/// with a daemon that looks alive but never works.
pub fn is_auth(err: &IntelError) -> bool {
    matches!(err, IntelError::Http(401 | 403, _))
}

/// HTTP statuses a **same-endpoint** retry may clear: a 429 rate-limit or an
/// upstream 5xx blip. This set must stay identical to the failover-class HTTP
/// split in [`classify`], or a status could be retried in place yet refuse to
/// fail over (or the reverse). A non-transient 4xx — bad request, auth — is a
/// caller error that is identical on a re-dial and must surface immediately.
pub fn is_transient_status(code: u16) -> bool {
    code == 429 || (500..600).contains(&code)
}

/// The result of one failover sweep, plus the side-channel of breaker and
/// active-endpoint transitions. The sweep observes these but emits nothing
/// itself; the caller turns them into metrics, events and the
/// `agentd://intelligence` body, which keeps this module free of observability
/// dependencies.
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

/// Drive one bounded failover sweep for a single logical `complete`. The sweep
/// visits at most `eps.len()` distinct endpoints and each of them at most once,
/// so one `complete` can never loop over the list.
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

    // Every breaker is OPEN and still cooling, so there is nothing to dial. The
    // caller turns this terminal into exit 4 in `once` mode, or into a backoff
    // and re-arm for a long-lived daemon.
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
        // A second-or-later attempt within one sweep IS a failover advance, and
        // is what the caller reports as such.
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

    // Every available endpoint failed over, so the whole list is down. The last
    // failover-class error is carried along as the cause, because the terminal
    // on its own tells an operator nothing about why.
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

    #[test]
    fn transient_status_matches_the_failover_class_split() {
        // The same-endpoint retry set (429 plus every 5xx) must match
        // `classify`'s failover-class HTTP codes exactly.
        for c in [429, 500, 502, 503, 504, 599] {
            assert!(is_transient_status(c), "{c} should be transient");
        }
        for c in [200, 400, 401, 403, 404, 418] {
            assert!(!is_transient_status(c), "{c} should NOT be transient");
        }
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

    /// Bind `127.0.0.1:0` and serve one response per element of `statuses` on
    /// successive connections — so a *same-endpoint* retry (which re-dials) walks
    /// into the next status in the list. Returns the URI.
    fn serve_sequence(statuses: Vec<u16>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for status in statuses {
                let Ok((mut s, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
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
        // One sweep over two dead endpoints exhausts the list, since each one
        // failed over, giving the all-down terminal that maps to exit 4 in
        // `once` mode.
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

    #[test]
    fn transient_5xx_is_retried_on_the_same_endpoint() {
        // One 503 then a 200 on a SINGLE endpoint: complete_once's same-endpoint
        // retry rides out the blip and succeeds in place, with no failover and
        // no exit 4. This matters most in once-mode, which arms no higher-level
        // retry loop, so without this retry a bare 503 would go straight to
        // exit 4.
        let ep = serve_sequence(vec![503, 200]);
        let mut list = list_of(&[ep]);
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(r.outcome.is_ok(), "same-endpoint retry cleared the 503");
        assert_eq!(r.served_by, Some(0));
        assert_eq!(r.failover, None, "handled in place, not failed over");
    }

    #[test]
    fn transient_429_is_retried_then_succeeds() {
        let ep = serve_sequence(vec![429, 200]);
        let mut list = list_of(&[ep]);
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(r.outcome.is_ok(), "429 rate-limit blip was retried");
        assert_eq!(r.served_by, Some(0));
    }

    #[test]
    fn non_transient_4xx_is_not_retried() {
        // 400 then 200 on one endpoint: because a 4xx is NOT retried, the first
        // (400) response surfaces immediately and the 200 is never consumed. If
        // the retry wrongly fired on 4xx, this would spuriously succeed.
        let ep = serve_sequence(vec![400, 200]);
        let mut list = list_of(&[ep]);
        let r = complete_resilient(&mut list, &req(), Duration::from_secs(2), None);
        assert!(
            matches!(r.outcome, Err(IntelError::Http(400, _))),
            "4xx must surface on the first dial, not be retried"
        );
    }
}

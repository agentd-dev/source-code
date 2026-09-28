// SPDX-License-Identifier: AGPL-3.0-only
//! Who is calling: the request's evidence, and the answers a request gets when
//! that evidence names nobody (401) or names somebody who may not (403).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::a2a::errors::{self, reason};
use crate::a2a::principals::{CertId, Evidence, SESSION_TOKEN_PREFIX, SessionVerifier};

/// The verified identity of the client certificate, when one was presented.
#[derive(Clone, Default, Debug)]
pub struct PeerId {
    pub presented: bool,
    pub subject: Option<String>,
    pub sans: Vec<String>,
}

pub(super) fn peer_identity(conn: &tokio_rustls::rustls::ServerConnection) -> PeerId {
    let Some(chain) = conn.peer_certificates() else {
        return PeerId::default();
    };
    let Some(leaf) = chain.first() else {
        return PeerId {
            presented: true,
            ..Default::default()
        };
    };
    let id = crate::net::x509::parse(leaf.as_ref());
    PeerId {
        presented: true,
        subject: id.subject_cn,
        sans: id.sans,
    }
}

/// Where the connection came from.
#[derive(Clone, Copy, Debug)]
pub(super) enum Peer {
    Tcp(SocketAddr),
    /// A unix-socket peer, already checked to be the daemon's own uid.
    Unix,
}

impl Peer {
    pub(super) fn is_unix(&self) -> bool {
        matches!(self, Peer::Unix)
    }

    /// The address failures are counted against. A unix peer has none, and
    /// never needs one: it is always the operator.
    pub(super) fn source(&self) -> Option<IpAddr> {
        match self {
            Peer::Tcp(a) => Some(a.ip()),
            Peer::Unix => None,
        }
    }
}

/// How the listener checks credentials beyond the rules the resolver holds.
#[derive(Default)]
pub struct Auth {
    /// The session store `agentd_at_` tokens are checked against. `None`
    /// until a store is installed, and then every session token fails.
    pub sessions: Option<Arc<dyn SessionVerifier + Send + Sync>>,
}

/// The request's evidence: its bearer, its verified certificate, whether the
/// peer is on this host, and whether a browser sent it.
pub(super) fn evidence_of(headers: &HeaderMap, peer_id: &PeerId, peer: &Peer) -> Evidence {
    Evidence {
        bearer: bearer_of(headers),
        cert: peer_id.presented.then(|| CertId {
            subject: peer_id.subject.clone(),
            sans: peer_id.sans.clone(),
        }),
        local: match peer {
            Peer::Tcp(a) => a.ip().to_canonical().is_loopback(),
            Peer::Unix => true,
        },
        // Presence, not value: `Origin: null` is what a `file://` page, a
        // sandboxed iframe or a `data:` URL sends, and it is as much a browser
        // as any named origin.
        origin: headers.contains_key(header::ORIGIN),
    }
}

/// The `Authorization: Bearer` token. The scheme is case-insensitive
/// (RFC 7235 §2.1), so `bearer`, `BEARER` and `Bearer` are one scheme.
fn bearer_of(headers: &HeaderMap) -> Option<String> {
    let h = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = h.trim().split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

/// Whether a presented bearer is a session token.
pub(super) fn is_session_token(ev: &Evidence) -> bool {
    ev.bearer
        .as_deref()
        .is_some_and(|b| b.starts_with(SESSION_TOKEN_PREFIX))
}

/// The browser's 401 on a listener that would have taken the same request,
/// without its `Origin`, as the operator.
pub(super) const BROWSER_MUST_AUTHENTICATE: &str = "browser requests must authenticate: sign in with the device grant, or open the UI with `agentd ui`";

/// The 401 challenge.
///
/// `presented`: a credential was offered and failed; `session`: it was a
/// session token; `browser`: nothing was presented and the request was
/// refused only because it came from a browser.
pub(super) fn challenge(presented: bool, session: bool, browser: bool) -> Response {
    let (www, message) = if presented {
        let description = if session {
            "the session token is unknown, expired or revoked"
        } else {
            "the credential is invalid or expired"
        };
        (
            format!(
                "Bearer realm=\"agentd\", error=\"invalid_token\", error_description=\"{description}\""
            ),
            "invalid or expired credential",
        )
    } else if browser {
        (
            "Bearer realm=\"agentd\"".to_string(),
            BROWSER_MUST_AUTHENTICATE,
        )
    } else {
        (
            "Bearer realm=\"agentd\"".to_string(),
            "authentication required",
        )
    };
    let body = errors::rpc_error(
        Value::Null,
        errors::UNAUTHENTICATED,
        message,
        vec![errors::error_info(
            errors::AGENTD_DOMAIN,
            reason::UNAUTHENTICATED,
            &[],
        )],
    );
    let mut resp = json_with(StatusCode::UNAUTHORIZED, &body);
    if let Ok(v) = HeaderValue::from_str(&www) {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
    }
    resp
}

/// The 403 refusal. `id` is echoed when the body was parsed; a bearer caller
/// is told the credential lacks the scope (RFC 6750 §3.1).
pub(super) fn forbidden(id: Value, message: &str, bearer_used: bool) -> Response {
    let body = errors::rpc_error(
        id,
        errors::PERMISSION_DENIED,
        message,
        vec![errors::error_info(
            errors::AGENTD_DOMAIN,
            reason::PERMISSION_DENIED,
            &[],
        )],
    );
    let mut resp = json_with(StatusCode::FORBIDDEN, &body);
    if bearer_used {
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"agentd\", error=\"insufficient_scope\""),
        );
    }
    resp
}

/// A 429: `Retry-After`, and the `RATE_LIMITED` reason with the same wait.
pub(super) fn too_many(id: Value, message: &str, retry_after: u64) -> Response {
    let secs = retry_after.to_string();
    let body = errors::rpc_error(
        id,
        errors::INTERNAL_ERROR,
        message,
        vec![errors::error_info(
            errors::AGENTD_DOMAIN,
            reason::RATE_LIMITED,
            &[("retryAfterSeconds", &secs)],
        )],
    );
    let mut resp = json_with(StatusCode::TOO_MANY_REQUESTS, &body);
    if let Ok(v) = HeaderValue::from_str(&secs) {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    resp
}

/// A JSON body with `status`. Every refusal is JSON — never an SSE frame, even
/// for a caller that asked for a stream — so it reads the same to every client.
pub(super) fn json_with(status: StatusCode, v: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(v).unwrap_or_default(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::a2a::Resolver;
    use crate::a2a::principals::{Resolution, Via};
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn local() -> Peer {
        Peer::Tcp("127.0.0.1:5000".parse().unwrap())
    }

    #[test]
    fn evidence_of_reads_the_scheme_case_insensitively() {
        for h in [
            "Bearer tok",
            "bearer tok",
            "BEARER tok",
            "BeArEr   tok  ",
            "Bearer\ttok",
        ] {
            let ev = evidence_of(
                &headers(&[("authorization", h)]),
                &PeerId::default(),
                &local(),
            );
            assert_eq!(ev.bearer.as_deref(), Some("tok"), "{h:?}");
        }
        for h in ["Basic dG9rOng=", "Bearer", "Bearer   ", "Bearertok"] {
            let ev = evidence_of(
                &headers(&[("authorization", h)]),
                &PeerId::default(),
                &local(),
            );
            assert_eq!(ev.bearer, None, "{h:?}");
        }
        let ev = evidence_of(&HeaderMap::new(), &PeerId::default(), &local());
        assert!(ev.local && !ev.origin && ev.cert.is_none());
        let remote = Peer::Tcp("10.1.2.3:5000".parse().unwrap());
        assert!(!evidence_of(&HeaderMap::new(), &PeerId::default(), &remote).local);
        let mapped = Peer::Tcp("[::ffff:127.0.0.1]:5000".parse().unwrap());
        assert!(evidence_of(&HeaderMap::new(), &PeerId::default(), &mapped).local);
        let cert = PeerId {
            presented: true,
            subject: Some("alice".into()),
            sans: vec![],
        };
        let ev = evidence_of(&HeaderMap::new(), &cert, &local());
        assert_eq!(ev.cert.unwrap().subject.as_deref(), Some("alice"));
    }

    /// `Origin: null` is a browser: a `file://` page, a sandboxed iframe or a
    /// `data:` URL sends exactly that. Reading the VALUE would let any of them
    /// be the implicit operator of a no-auth loopback daemon.
    #[test]
    fn origin_null_counts_as_an_origin() {
        let ev = evidence_of(
            &headers(&[("origin", "null")]),
            &PeerId::default(),
            &local(),
        );
        assert!(ev.origin);
        let r = Resolver::build(
            &serde_json::from_value(json!({"listen": "http://127.0.0.1:8080"})).unwrap(),
            &|_| None,
        )
        .unwrap();
        let got = r.resolve(&ev, false, None);
        assert!(
            !matches!(got, Resolution::Named(_, Via::Implicit)),
            "Origin: null was the implicit operator: {got:?}"
        );
        assert_eq!(got, Resolution::Unauthenticated { presented: false });
    }

    #[test]
    fn the_challenge_and_refusal_shapes() {
        let www = |r: &Response| {
            r.headers()
                .get(header::WWW_AUTHENTICATE)
                .map(|v| v.to_str().unwrap().to_string())
        };
        let r = challenge(false, false, false);
        assert_eq!(r.status(), 401);
        assert_eq!(www(&r).as_deref(), Some("Bearer realm=\"agentd\""));
        let r = challenge(true, false, false);
        assert!(www(&r).unwrap().contains("error=\"invalid_token\""));
        assert!(
            www(&r)
                .unwrap()
                .contains("the credential is invalid or expired")
        );
        let r = challenge(true, true, false);
        assert!(www(&r).unwrap().contains("unknown, expired or revoked"));
        let r = forbidden(json!(7), "x is not permitted for user:a", true);
        assert_eq!(r.status(), 403);
        assert!(www(&r).unwrap().contains("insufficient_scope"));
        assert_eq!(www(&forbidden(json!(7), "m", false)), None);
        let r = too_many(Value::Null, "slow down", 3);
        assert_eq!(r.status(), 429);
        assert_eq!(r.headers().get(header::RETRY_AFTER).unwrap(), "3");
    }
}

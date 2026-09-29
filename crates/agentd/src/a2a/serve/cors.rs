// SPDX-License-Identifier: AGPL-3.0-only
//! Browser origins: the preflight, the grant on a real response, and the
//! allowlist both consult.
//!
//! One policy for every credentialed route (`POST /`, its preflight, and the
//! OAuth endpoints): an `Origin` is parsed and matched EXACTLY against
//! `a2a.cors.origins`. Nothing is trusted for where it is — a page on another
//! loopback port is as foreign as one on the internet, because any page a
//! local browser loads can make it send a request to 127.0.0.1. And admission
//! grants nothing by itself: a listed origin's request still authenticates.
//! The public card has its own, wider policy (see `card.rs`).

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::App;
use crate::config::settings::parse_origin;

/// The request headers a browser client may send, answered on the preflight.
///
/// None of them is CORS-safelisted, so each one missing here is a call a
/// browser refuses to make: `a2a-version` rides on every A2A 1.0 request, and
/// without `a2a-extensions` a browser client could never activate one.
pub(super) const CORS_REQUEST_HEADERS: &[&str] = &[
    "content-type",
    "authorization",
    "last-event-id",
    "a2a-extensions",
    "a2a-version",
];

/// The response headers page script may read, on every actual response.
///
/// A browser reads exposure from the response itself, never from the
/// preflight, so these ride on the 200s and on the refusals alike: the
/// activated extensions, how long a 429 asks it to wait, and the challenge a
/// 401 or 403 carries — without which a page cannot tell "sign in" from
/// "not allowed".
pub(super) const CORS_EXPOSED_HEADERS: &[&str] =
    &["a2a-extensions", "retry-after", "www-authenticate"];

/// The origin a request names, if it names one this listener could read.
pub(super) fn origin_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
}

/// The origin gate every credentialed route runs before anything else:
/// `Ok(None)` for a request with no `Origin` (not a browser: nothing to
/// admit, nothing to grant), `Ok(Some(origin))` for a listed one, and for
/// every other the one refusal ([`OriginGate`]) — an `Origin` that is present
/// but unreadable included. A browser never sends one, but the resolver
/// counts a request that carries the header as a browser's, and a gate that
/// read it as absent would wave through what identity treats as a page.
pub(super) fn gate(headers: &HeaderMap, allowed: &[String]) -> Result<Option<String>, OriginGate> {
    let Some(raw) = headers.get(header::ORIGIN) else {
        return Ok(None);
    };
    match raw.to_str() {
        Ok(o) if origin_allowed(o, allowed) => Ok(Some(o.to_string())),
        _ => Err(OriginGate),
    }
}

/// The origin gate said no; answered as [`refused`].
#[derive(Debug)]
pub(super) struct OriginGate;

impl IntoResponse for OriginGate {
    fn into_response(self) -> Response {
        refused()
    }
}

/// A CORS preflight. A browser UI served from a configured origin has to be
/// told it may POST here; every other origin is refused, which is the same
/// DNS-rebind answer the POST itself gives.
///
/// **Private Network Access.** A page on a public origin — a hosted UI at
/// `https://code.agentd.dev` — reaching a daemon on loopback or a LAN address
/// is exactly the shape browsers now gate: Chrome sends
/// `Access-Control-Request-Private-Network: true` on the preflight and drops
/// the real request unless the answer carries
/// `Access-Control-Allow-Private-Network: true`. Without it a hosted client
/// fails with a CORS error that names no cause, which is a miserable thing to
/// debug from the outside.
///
/// The grant is deliberately narrow: it rides on the SAME origin allow-list as
/// everything else, so answering it says only "the origin you already
/// configured may reach this daemon", never "any website may". An origin that
/// is not in `a2a.cors.origins` is refused before this header is ever
/// considered.
pub(super) async fn preflight(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    preflight_for(&app.origins(), &headers)
}

/// [`preflight`] against an allowlist in hand.
fn preflight_for(origins: &[String], headers: &HeaderMap) -> Response {
    let origin = origin_of(headers).unwrap_or("");
    if !origin_allowed(origin, origins) {
        return refused();
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("POST, GET, OPTIONS"),
    );
    if let Ok(v) = HeaderValue::from_str(&CORS_REQUEST_HEADERS.join(", ")) {
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
    }
    h.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    h.append(header::VARY, HeaderValue::from_static("Origin"));
    if wants_private_network(headers) {
        h.insert(
            "access-control-allow-private-network",
            HeaderValue::from_static("true"),
        );
    }
    resp
}

/// Whether a preflight asks for Private Network Access.
pub(super) fn wants_private_network(headers: &HeaderMap) -> bool {
    headers
        .get("access-control-request-private-network")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// The refusal an origin outside the allowlist gets: 403, no body, and no
/// grant — so the page that asked cannot even read that it was refused.
/// `Vary: Origin` still goes out, because a listed origin asking the same
/// thing is answered differently and a cache must not hand one the other's.
/// It carries [`OriginRefused`], which is how [`allow_origin`] knows it.
fn refused() -> Response {
    let mut resp = StatusCode::FORBIDDEN.into_response();
    resp.headers_mut()
        .append(header::VARY, HeaderValue::from_static("Origin"));
    resp.extensions_mut().insert(OriginRefused);
    resp
}

/// Marks the origin gate's own refusal. A 403 is otherwise a caller's
/// refusal, which a listed origin must be able to read; telling the two apart
/// by anything the response says for itself (its status, its content type)
/// would strip the next route's 403 of its body and grant without a word.
#[derive(Debug, Clone, Copy)]
struct OriginRefused;

/// Grant the caller's origin on a real response, so the browser hands the body
/// — and the headers a page needs to act on it — to the page that asked.
///
/// Every request that reaches here with an `Origin` has already passed the
/// origin gate but one: the gate's own refusal ([`gate`]), which goes out as
/// it is — no grant, no body — so a refused origin is never told what it was
/// refused, while the 401 or 403 a LISTED origin gets still carries the grant
/// and its challenge, which is how a page learns it has to sign in.
pub(super) fn allow_origin(mut resp: Response, origin: Option<&str>) -> Response {
    if resp.extensions().get::<OriginRefused>().is_some() {
        return resp;
    }
    let Some(origin) = origin else {
        resp.headers_mut()
            .append(header::VARY, HeaderValue::from_static("Origin"));
        return resp;
    };
    let h = resp.headers_mut();
    h.append(header::VARY, HeaderValue::from_static("Origin"));
    if let Ok(v) = HeaderValue::from_str(origin) {
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        if let Ok(v) = HeaderValue::from_str(&CORS_EXPOSED_HEADERS.join(", ")) {
            h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, v);
        }
    }
    resp
}

/// Whether `origin` is one of `allowed`, compared as origins: scheme, host
/// (case-folded, IPv6 unbracketed) and port (the scheme's default applied),
/// so `https://UI.example:443` is `https://ui.example` and `http://[::1]:4173`
/// is itself rather than a host named `[`.
///
/// Nothing matches that is not listed. `Origin: null` — what a sandboxed
/// iframe, a `file:` page or a redirect chain sends — and anything that does
/// not parse as an origin match nothing, and a `*` in the list (refused at
/// load) would match nothing either: it is not an origin.
pub(super) fn origin_allowed(origin: &str, allowed: &[String]) -> bool {
    let Ok(origin) = parse_origin(origin) else {
        return false;
    };
    allowed
        .iter()
        .any(|a| parse_origin(a).is_ok_and(|a| a == origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    fn header_of<'r>(resp: &'r Response, name: &str) -> Option<&'r str> {
        resp.headers().get(name).and_then(|v| v.to_str().ok())
    }

    fn varies_on_origin(resp: &Response) -> bool {
        resp.headers()
            .get_all(header::VARY)
            .iter()
            .any(|v| v.to_str().is_ok_and(|v| v.eq_ignore_ascii_case("origin")))
    }

    /// The origin policy is an exact match of parsed origins and nothing else:
    /// no loopback port is trusted for being loopback, `null` and junk never
    /// match, and a bracketed IPv6 origin is compared as one. A refused origin
    /// is told nothing (403, no grant, no body, still `Vary: Origin`); a
    /// listed one is granted with the headers its page needs on every actual
    /// response, refusals included.
    #[tokio::test]
    async fn cors_policy() {
        let allowed: Vec<String> = [
            "https://ui.example",
            "http://[::1]:4173",
            "http://127.0.0.1:4173",
        ]
        .map(String::from)
        .to_vec();
        for yes in [
            "https://ui.example",
            "HTTPS://UI.Example:443",
            "http://[::1]:4173",
            "http://[0:0:0:0:0:0:0:1]:4173",
            "http://127.0.0.1:4173",
        ] {
            assert!(origin_allowed(yes, &allowed), "{yes} should be admitted");
        }
        for no in [
            // Implicit loopback trust is gone: listed ports only.
            "http://127.0.0.1:9999",
            "http://localhost:4173",
            "http://[::1]:9999",
            "http://127.0.0.2:4173",
            // Scheme and port are part of the origin.
            "http://ui.example",
            "https://ui.example:8443",
            // Neither a suffix nor a prefix is the origin.
            "https://ui.example.evil.com",
            "https://evil-ui.example",
            "null",
            "",
            "*",
            "ui.example",
            "https://ui.example/",
        ] {
            assert!(!origin_allowed(no, &allowed), "{no:?} was admitted");
        }
        // A `*` that got past load is not a wildcard.
        assert!(!origin_allowed("https://ui.example", &["*".to_string()]));
        assert!(!origin_allowed("null", &["null".to_string()]));

        // The preflight: a listed origin gets the grant and every header a
        // call carries…
        let resp = preflight_for(
            &allowed,
            &headers(&[
                ("origin", "http://[::1]:4173"),
                ("access-control-request-method", "POST"),
            ]),
        );
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            header_of(&resp, "access-control-allow-origin"),
            Some("http://[::1]:4173")
        );
        assert_eq!(
            header_of(&resp, "access-control-allow-headers"),
            Some("content-type, authorization, last-event-id, a2a-extensions, a2a-version")
        );
        assert_eq!(
            header_of(&resp, "access-control-allow-methods"),
            Some("POST, GET, OPTIONS")
        );
        assert_eq!(header_of(&resp, "access-control-max-age"), Some("600"));
        assert!(varies_on_origin(&resp));
        assert_eq!(
            header_of(&resp, "access-control-allow-private-network"),
            None,
            "PNA is not volunteered"
        );
        let resp = preflight_for(
            &allowed,
            &headers(&[
                ("origin", "https://ui.example"),
                ("access-control-request-private-network", "true"),
            ]),
        );
        assert_eq!(
            header_of(&resp, "access-control-allow-private-network"),
            Some("true")
        );
        // …an unlisted one, loopback or not, is refused whatever it asks.
        for origin in ["http://127.0.0.1:9999", "https://evil.example", "null"] {
            let resp = preflight_for(
                &allowed,
                &headers(&[
                    ("origin", origin),
                    ("access-control-request-private-network", "true"),
                ]),
            );
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{origin}");
            assert_eq!(header_of(&resp, "access-control-allow-origin"), None);
            assert_eq!(
                header_of(&resp, "access-control-allow-private-network"),
                None
            );
            assert!(varies_on_origin(&resp), "{origin}");
        }

        // An actual response to a listed origin: granted, with what a page
        // reads — on a refusal of the caller as much as on a success.
        let challenge = (
            StatusCode::UNAUTHORIZED,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::WWW_AUTHENTICATE, "Bearer"),
            ],
            json!({"jsonrpc": "2.0", "id": null, "error": {"code": -31401}}).to_string(),
        )
            .into_response();
        let caller_403 = (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            "{}",
        )
            .into_response();
        // A caller's 403 in any other shape is still the caller's.
        let problem_403 = (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/problem+json")],
            "{}",
        )
            .into_response();
        let text_403 = (StatusCode::FORBIDDEN, "not yours").into_response();
        for resp in [
            StatusCode::OK.into_response(),
            challenge,
            caller_403,
            problem_403,
            text_403,
        ] {
            let status = resp.status();
            let resp = allow_origin(resp, Some("https://ui.example"));
            assert_eq!(resp.status(), status);
            assert_eq!(
                header_of(&resp, "access-control-allow-origin"),
                Some("https://ui.example"),
                "{status}"
            );
            assert_eq!(
                header_of(&resp, "access-control-expose-headers"),
                Some("a2a-extensions, retry-after, www-authenticate"),
                "{status}"
            );
            assert!(varies_on_origin(&resp), "{status}");
        }
        // The gate: no Origin passes as no browser, a listed one passes as
        // itself, and an unlisted one — or one that is present but not
        // readable, which the resolver still counts as a browser's — gets the
        // gate's refusal, which goes out with no grant and no body.
        assert_eq!(gate(&HeaderMap::new(), &allowed).ok(), Some(None));
        assert_eq!(
            gate(&headers(&[("origin", "https://ui.example")]), &allowed).ok(),
            Some(Some("https://ui.example".to_string()))
        );
        let mut unreadable = HeaderMap::new();
        unreadable.insert(
            header::ORIGIN,
            HeaderValue::from_bytes(b"https://ui.example\xff").unwrap(),
        );
        for (what, h) in [
            ("unlisted", headers(&[("origin", "https://evil.example")])),
            ("unreadable", unreadable),
        ] {
            let Err(refusal) = gate(&h, &allowed) else {
                panic!("{what} origin passed the gate");
            };
            let resp = allow_origin(refusal.into_response(), origin_of(&h));
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{what}");
            assert_eq!(header_of(&resp, "access-control-allow-origin"), None);
            assert_eq!(header_of(&resp, "access-control-expose-headers"), None);
            assert!(varies_on_origin(&resp), "{what}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert!(body.is_empty(), "{what}: {body:?}");
        }
        // No Origin: not a browser, nothing to grant.
        let resp = allow_origin(StatusCode::OK.into_response(), None);
        assert_eq!(header_of(&resp, "access-control-allow-origin"), None);
    }
}

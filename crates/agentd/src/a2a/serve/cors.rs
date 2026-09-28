// SPDX-License-Identifier: AGPL-3.0-only
//! Browser origins: the preflight, the grant on a real response, and the
//! allowlist both consult.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::App;

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
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !origin_allowed(origin, &app.origins()) {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    let wants_private_network = headers
        .get("access-control-request-private-network")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let mut resp = (
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.to_string()),
            (
                header::ACCESS_CONTROL_ALLOW_METHODS,
                "POST, GET, OPTIONS".to_string(),
            ),
            (
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                // `a2a-extensions` rides here too, or a browser client could
                // never activate one: the preflight would reject the header
                // before the request that carries it is ever sent. The same
                // holds for `a2a-version`, which every A2A 1.0 client sends on
                // every call — without it a browser can make no call at all.
                "content-type, authorization, last-event-id, a2a-extensions, a2a-version"
                    .to_string(),
            ),
            (
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
                "a2a-extensions".to_string(),
            ),
            (header::ACCESS_CONTROL_MAX_AGE, "600".to_string()),
        ],
    )
        .into_response();
    if wants_private_network {
        resp.headers_mut().insert(
            "access-control-allow-private-network",
            axum::http::HeaderValue::from_static("true"),
        );
    }
    resp
}

/// Grant the caller's origin on a real response, so the browser hands the body
/// to the page that asked for it.
pub(super) fn allow_origin(mut resp: Response, origin: Option<&str>) -> Response {
    if let Some(o) = origin
        && let Ok(v) = axum::http::HeaderValue::from_str(o)
    {
        resp.headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
    }
    resp
}

/// Loopback origins are always allowed; anything else must be configured.
pub(super) fn origin_allowed(origin: &str, extra: &[String]) -> bool {
    if extra.iter().any(|o| o == origin || o == "*") {
        return true;
    }
    let host = origin
        .split("://")
        .nth(1)
        .unwrap_or(origin)
        .split(':')
        .next()
        .unwrap_or("");
    crate::net::http::is_loopback_host(host)
}

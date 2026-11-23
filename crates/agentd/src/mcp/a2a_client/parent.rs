// SPDX-License-Identifier: AGPL-3.0-only
//! The parent link: an instance child's reports, through the channel it was
//! handed at spawn ([`crate::a2a::channel`]).
//!
//! The one request agentd still frames itself. The channel is a door with no
//! path: each connection is a fresh socket pair whose other end is passed
//! through it, and the SDK's `reqwest` client can neither dial a socket that
//! has no path nor be handed one (its connector must yield `reqwest`'s own
//! `Conn`). What goes through is what the parent accepts from a child and
//! nothing more: one `SendMessage` carrying an `_instance.result` or
//! `_instance.emit` report, its version and extensions stated, its result
//! read back. No card, no stream, no poll.

use super::{PeerAuth, request_timeout};
use crate::runtime::surface::A2A_PROTOCOL_VERSION;
use serde_json::Value;
use std::time::Instant;

/// `params` (a `SendMessage`'s, as the wire carries them) to the parent: its
/// JSON-RPC `result`, or why there is none.
#[cfg(unix)]
pub(super) fn send(params: Value, auth: &PeerAuth, deadline: Instant) -> Result<Value, String> {
    use ::mcp::rpc::{Id, Request, Response};
    let marked = crate::a2a::peer::extensions_of(&params);
    let req = Request::new(Id::Num(1), "SendMessage", Some(params));
    let body = serde_json::to_vec(&req).map_err(|e| format!("a2a: encode SendMessage: {e}"))?;
    let mut headers: Vec<(String, String)> = vec![
        ("Content-Type".into(), "application/json".into()),
        ("A2A-Version".into(), A2A_PROTOCOL_VERSION.into()),
    ];
    if !marked.is_empty() {
        headers.push(("A2A-Extensions".into(), marked.join(", ")));
    }
    if let Some(t) = &auth.trace {
        headers.extend(t.headers());
    }
    headers.extend(auth.headers.iter().cloned());
    let headers: Vec<(&str, &str)> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut stream = crate::a2a::channel::connect(request_timeout(deadline))
        .map_err(|e| format!("a2a: cannot reach the parent through its channel: {e}"))?;
    let resp = crate::net::http::send(&mut stream, "localhost", "POST", "/", &headers, &body)
        .map_err(|e| match e.kind() {
            // Linux reports an expired SO_RCVTIMEO as EAGAIN, not TimedOut.
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                "a2a: SendMessage: timed out waiting for the parent".to_string()
            }
            _ => format!("a2a: SendMessage: {e}"),
        })?;
    if !resp.is_success() {
        return Err(format!("a2a: SendMessage HTTP {}", resp.status));
    }
    let response: Response = serde_json::from_slice(&resp.body)
        .map_err(|e| format!("a2a: SendMessage bad reply: {e}"))?;
    if let Some(err) = response.error {
        return Err(format!(
            "a2a: SendMessage rpc error {}: {}",
            err.code, err.message
        ));
    }
    Ok(response.result.unwrap_or(Value::Null))
}

/// No channel off unix: an instance child is spawned with one only there.
#[cfg(not(unix))]
pub(super) fn send(_params: Value, _auth: &PeerAuth, _deadline: Instant) -> Result<Value, String> {
    Err("a2a: an instance child reaches its parent through a unix channel".to_string())
}

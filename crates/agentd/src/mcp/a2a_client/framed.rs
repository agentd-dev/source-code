// SPDX-License-Identifier: AGPL-3.0-only
//! The one-request sender: a JSON-RPC request agentd frames itself, for the
//! three requests the SDK's client cannot carry.
//!
//! * **The parent link** ([`to_parent`]) — an instance child's
//!   `_instance.result` / `_instance.emit` report, through the channel it was
//!   handed at spawn ([`crate::a2a::channel`]). The channel is a door with
//!   no path: each connection is a fresh socket pair whose other end is
//!   passed through it, and `reqwest` can neither dial a socket that has no
//!   path nor be handed one (its connector must yield `reqwest`'s own
//!   `Conn`). The parent accepts nothing else from a child.
//! * **An x402 payment** ([`to_peer`], from `paying`) — the message whose
//!   metadata carries the `PaymentPayload`. The released `a2a-pb` writes
//!   every whole number in a metadata `Struct` as a double (`x402Version: 2`
//!   as `2.0`), which a strict verifier refuses, so the payment travels
//!   byte for byte as built. **Delete this use when `a2a-pb` releases
//!   90e2876** (see [`crate::a2a::sdk_gaps`]) and send the payment through
//!   the SDK like every other request.
//! * **An agentd extension method** ([`to_peer`], from
//!   [`super::call_extension`]) — `agentd.operator/<op>` on an agentd's own
//!   listener, as `agentd --login company:<name>` and the `--delegation-*`
//!   clients call it. The SDK's client sends the core methods it knows and
//!   has no call for a method an extension declares (A2A §4.6.2), so a
//!   method agentd declares is sent here: a JSON-RPC request and its
//!   response, and nothing of the protocol beside them. Not an SDK defect —
//!   this use stays when the payment's goes.
//!
//! One request per connection: the version and the caller's headers stated,
//! the JSON-RPC envelope written, its `result` read back — no card, no
//! stream, no poll.

use super::HttpEp;
use super::{PeerAuth, request_timeout};
use crate::runtime::surface::A2A_PROTOCOL_VERSION;
use serde_json::Value;
use std::time::Instant;

/// How far a framed request got.
pub(super) enum Failure {
    /// It never left: no connection, or nothing to send.
    Unsent(String),
    /// It left and no readable answer came, or the peer failed inside
    /// (an HTTP 5xx, a JSON-RPC internal error): it may have been acted on.
    Unanswered(String),
    /// The peer did not take it: an HTTP 4xx, or another JSON-RPC error.
    Refused(String),
}

impl Failure {
    pub(super) fn into_message(self) -> String {
        match self {
            Failure::Unsent(e) | Failure::Unanswered(e) | Failure::Refused(e) => e,
        }
    }
}

/// `params` (a `SendMessage`'s, as the wire carries them) to the parent
/// through this instance child's channel: its `result`, or why there is none.
pub(super) fn to_parent(
    params: Value,
    auth: &PeerAuth,
    deadline: Instant,
) -> Result<Value, String> {
    #[cfg(unix)]
    {
        let marked = crate::a2a::peer::extensions_of(&params);
        let mut headers = Vec::new();
        if !marked.is_empty() {
            headers.push(("A2A-Extensions".to_string(), marked.join(", ")));
        }
        if let Some(t) = &auth.trace {
            headers.extend(t.headers());
        }
        headers.extend(auth.headers.iter().cloned());
        let stream = crate::a2a::channel::connect(request_timeout(deadline))
            .map_err(|e| format!("a2a: cannot reach the parent through its channel: {e}"))?;
        send_request(stream, "localhost", "/", &headers, "SendMessage", params)
            .map_err(Failure::into_message)
    }
    #[cfg(not(unix))]
    {
        let _ = (params, auth, deadline);
        Err("a2a: an instance child reaches its parent through a unix channel".to_string())
    }
}

/// `method` with `params` to the peer at `ep`, presenting `headers` (the
/// connection's extensions, trace and credential) and `auth`'s mTLS
/// identity.
pub(super) fn to_peer(
    ep: &HttpEp,
    auth: &PeerAuth,
    headers: &[(String, String)],
    method: &str,
    params: Value,
    deadline: Instant,
) -> Result<Value, Failure> {
    let timeout = request_timeout(deadline);
    let stream: Box<dyn crate::net::http::Stream> = if let Some(socket) = &ep.socket {
        Box::new(
            crate::net::unixsock::connect(socket, timeout).map_err(|e| {
                Failure::Unsent(format!("a2a: cannot reach peer socket {socket}: {e}"))
            })?,
        )
    } else {
        let tcp = crate::net::http::connect_tcp(&ep.host, ep.port, timeout)
            .map_err(|e| Failure::Unsent(format!("a2a: cannot reach peer {}: {e}", ep.host)))?;
        if ep.tls {
            Box::new(
                crate::net::tls::connect(tcp, &ep.host, auth.identity.as_ref())
                    .map_err(|e| Failure::Unsent(format!("a2a: tls to peer {}: {e}", ep.host)))?,
            )
        } else {
            Box::new(tcp)
        }
    };
    send_request(stream, &ep.host_header, &ep.path, headers, method, params)
}

/// One JSON-RPC `method` call with `params` on `stream`.
fn send_request<S: std::io::Read + std::io::Write>(
    mut stream: S,
    host_header: &str,
    path: &str,
    headers: &[(String, String)],
    method: &str,
    params: Value,
) -> Result<Value, Failure> {
    use ::mcp::rpc::{INTERNAL_ERROR, Id, Request, Response};
    let req = Request::new(Id::Num(1), method, Some(params));
    let body = serde_json::to_vec(&req)
        .map_err(|e| Failure::Unsent(format!("a2a: encode {method}: {e}")))?;
    let mut all: Vec<(&str, &str)> = vec![
        ("Content-Type", "application/json"),
        ("A2A-Version", A2A_PROTOCOL_VERSION),
    ];
    all.extend(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let resp = crate::net::http::send(&mut stream, host_header, "POST", path, &all, &body)
        .map_err(|e| {
            Failure::Unanswered(match e.kind() {
                // Linux reports an expired SO_RCVTIMEO as EAGAIN, not TimedOut.
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    format!("a2a: {method}: timed out waiting for the peer")
                }
                _ => format!("a2a: {method}: {e}"),
            })
        })?;
    let status = format!("a2a: {method} HTTP {}", resp.status);
    if resp.status >= 500 {
        return Err(Failure::Unanswered(status));
    }
    if !resp.is_success() {
        return Err(Failure::Refused(status));
    }
    let response: Response = serde_json::from_slice(&resp.body)
        .map_err(|e| Failure::Unanswered(format!("a2a: {method} bad reply: {e}")))?;
    if let Some(err) = response.error {
        let why = format!("a2a: {method} rpc error {}: {}", err.code, err.message);
        return Err(if err.code == INTERNAL_ERROR {
            Failure::Unanswered(why)
        } else {
            Failure::Refused(why)
        });
    }
    Ok(response.result.unwrap_or(Value::Null))
}

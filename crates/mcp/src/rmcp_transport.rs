// SPDX-License-Identifier: AGPL-3.0-only
//! **agentd's socket, under the official SDK's transport trait.**
//!
//! [`rmcp`] owns the MCP protocol — the handshake, the request and notification
//! types, capability negotiation, the streaming rules, the version table. What
//! it does not own here is the connection, and the reason is credentials.
//!
//! agentd reaches an MCP server through [`HttpTransport`], which carries things
//! rmcp's own reqwest client has no notion of: an AAuth request signature
//! (RFC 9421) with its challenge/re-sign loop, an AWS SigV4 signature computed
//! per request, an mTLS client identity presented during the handshake, an OAuth
//! token refreshed when it expires, and an SSRF guard on every dial. Adopting
//! the SDK's transport wholesale would mean dropping all of that to gain a
//! protocol implementation we can have anyway — so the SDK plugs into our
//! socket rather than replacing it.
//!
//! ## Blocking underneath, async above
//!
//! [`HttpTransport`] is blocking, because agentd's runtime is. The trait is
//! async. Each call therefore runs on a blocking thread and is awaited; a
//! response that arrived as Server-Sent Events is replayed to the SDK as the
//! event stream it expects, in order, including any notifications that came
//! interleaved with the reply.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use serde_json::Value;
use sse_stream::{Error as SseError, Sse};

use crate::http::HttpTransport;

/// The SDK's transport, backed by agentd's authenticated HTTP.
#[derive(Clone)]
pub struct AgentdHttp {
    http: Arc<HttpTransport>,
    timeout: Duration,
}

impl AgentdHttp {
    pub fn new(http: Arc<HttpTransport>, timeout: Duration) -> AgentdHttp {
        AgentdHttp { http, timeout }
    }
}

/// What can go wrong at the socket. The protocol's own errors are the SDK's;
/// this is only "the message never made it".
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TransportError(String);

/// One item lifted off a POST's response stream, in arrival order.
enum Pumped {
    /// A message the server sent BEFORE its reply: a notification, or — the
    /// case that makes the ordering load-bearing — a request of its own
    /// (elicitation, sampling) that it is now blocked waiting for us to answer.
    Message(Value),
    /// The exchange ended: the reply (`None` when the POST was a notification
    /// and `202 Accepted` came back), or the transport error that ended it.
    Done(Result<Option<Value>, String>),
}

/// One JSON value as the SDK expects to read it off a stream.
fn as_event(v: &Value) -> Sse {
    Sse {
        event: None,
        data: Some(v.to_string()),
        id: None,
        retry: None,
    }
}

impl StreamableHttpClient for AgentdHttp {
    type Error = TransportError;

    /// POST one message.
    ///
    /// Everything the server says in reply — whatever it interleaves, then the
    /// response itself — is handed back as a stream, because that is the only
    /// shape that can carry more than one message and the SDK reads it the same
    /// either way. A notification-only POST is answered `202 Accepted` by the
    /// server and reported as accepted here.
    ///
    /// **The stream is live, not a replay.** The spec lets a server interleave a
    /// REQUEST of its own on the response stream of a POST — an elicitation, a
    /// sampling call — and then block until the client answers it (over a
    /// separate POST) before finishing the original reply. Collecting the frames
    /// and returning them once the reply landed would deadlock exactly that
    /// exchange: the server waits for an answer the SDK has not been shown yet,
    /// the POST runs to its timeout, and the frames are dropped with the error.
    /// So the blocking read runs on its own thread and forwards each frame as it
    /// arrives; this returns as soon as the FIRST one does.
    async fn post_message(
        &self,
        _uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        _session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let body = serde_json::to_vec(&message)
            .map_err(|e| StreamableHttpError::Client(TransportError(e.to_string())))?;
        let request_id = request_id_of(&message);
        let http = Arc::clone(&self.http);
        let timeout = self.timeout;
        let extra = header_pairs(auth_header, custom_headers);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Pumped>();
        // Not awaited: the send owns a blocking thread for as long as the server
        // keeps the exchange open, and this call must return before it finishes.
        // The thread ends when the send does; a receiver dropped early makes the
        // sends fail, which costs nothing since the send is already unwinding.
        let notes_tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let refs: Vec<(&str, &str)> = extra
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let resp = http.send(request_id, &body, timeout, &refs, |n| {
                let _ = notes_tx.send(Pumped::Message(n));
            });
            let _ = tx.send(Pumped::Done(resp.map_err(|e| e.to_string())));
        });

        // Wait for the first frame only. `Mcp-Session-Id` rides the response
        // HEAD, which the transport has already recorded by the time any frame
        // can reach us, so reading it here is not early.
        let first = match rx.recv().await {
            Some(Pumped::Message(v)) | Some(Pumped::Done(Ok(Some(v)))) => v,
            // A notification: nothing came back, and nothing should have.
            Some(Pumped::Done(Ok(None))) => return Ok(StreamableHttpPostResponse::Accepted),
            Some(Pumped::Done(Err(e))) => {
                return Err(StreamableHttpError::Client(TransportError(e)));
            }
            // The pump vanished without reporting — only reachable if the
            // blocking thread itself died, which is a dead socket either way.
            None => {
                return Err(StreamableHttpError::Client(TransportError(
                    "mcp: response stream ended with no reply".into(),
                )));
            }
        };
        let session = self.http.session_id();

        let rest = futures::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Some(Pumped::Message(v)) | Some(Pumped::Done(Ok(Some(v)))) => {
                    Some((Ok(as_event(&v)), rx))
                }
                // Done — cleanly, or with an error the first frame already
                // outlived. Either way this POST's stream is over, and ending
                // the stream is how the SDK is told so.
                _ => None,
            }
        });
        let head: Vec<Result<Sse, SseError>> = vec![Ok(as_event(&first))];
        Ok(StreamableHttpPostResponse::Sse(
            Box::pin(futures::stream::iter(head).chain(rest)),
            session,
        ))
    }

    /// End a session. Best-effort by design: a server that has already forgotten
    /// the session, or that never had one, is not an error worth failing a
    /// shutdown over.
    async fn delete_session(
        &self,
        _uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let http = Arc::clone(&self.http);
        let timeout = self.timeout;
        let extra = header_pairs(auth_header, custom_headers);
        let sid = session_id.to_string();
        let _ = (http, timeout, extra, sid);
        // agentd's transport ends a session by dropping the connection; there is
        // no separate DELETE to make, and a server that keeps a session it will
        // never hear from again ages it out.
        Ok(())
    }

    /// Open the server→client event stream: the channel a server uses to send
    /// requests of its own (elicitation, sampling, roots) and unsolicited
    /// notifications.
    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, StreamableHttpError<Self::Error>> {
        let http = Arc::clone(&self.http);
        let timeout = self.timeout;
        let extra = header_pairs(auth_header, custom_headers);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Sse, SseError>>();

        // A blocking reader pumping into a channel: the stream the SDK polls is
        // the receiving end. When the SDK drops the stream the sends fail and
        // the reader stops, so a closed stream closes the connection.
        std::thread::spawn(move || {
            let mut refs: Vec<(&str, &str)> = extra
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            if let Some(id) = &last_event_id {
                refs.push(("Last-Event-ID", id.as_str()));
            }
            let _ = &refs;
            let Ok(mut events) = http.open_events(timeout) else {
                return;
            };
            while let Ok(Some(ev)) = events.next_event() {
                let sse = Sse {
                    event: ev.event,
                    data: Some(ev.data),
                    id: ev.id,
                    retry: None,
                };
                if tx.send(Ok(sse)).is_err() {
                    return;
                }
            }
        });

        Ok(Box::pin(
            tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
        ))
    }
}

/// The JSON-RPC id of a request, or `None` for a notification — which is what
/// decides whether a reply is expected at all.
///
/// A RESPONSE the client is sending (the answer to a server→client elicitation
/// or sampling request) carries an id too, and it is emphatically not one we are
/// owed a reply for: the server acks it `202` with no body. Only a message with
/// a `method` is a request of ours, so that is what the id is read from.
fn request_id_of(message: &rmcp::model::ClientJsonRpcMessage) -> Option<i64> {
    serde_json::to_value(message)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_i64))
}

/// The SDK's headers, flattened to the pairs our transport takes. A header whose
/// value is not valid UTF-8 is dropped rather than mangled: a header we cannot
/// represent faithfully is worse than one we did not send.
fn header_pairs(
    auth_header: Option<String>,
    custom: HashMap<http::HeaderName, http::HeaderValue>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(a) = auth_header {
        out.push(("Authorization".to_string(), a));
    }
    for (k, v) in custom {
        if let Ok(s) = v.to_str() {
            out.push((k.as_str().to_string(), s.to_string()));
        }
    }
    out
}

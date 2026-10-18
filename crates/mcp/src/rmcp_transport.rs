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
//! token refreshed when it expires. Adopting
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
//!
//! ## The server's answer, whatever carries it
//!
//! A server's JSON-RPC answer is the request's answer in any body: a `400` or
//! `500` whose body is a JSON-RPC error, or an error frame on an event stream
//! whose id the server could not fill in. The SDK routes answers by id, so an
//! error whose id is wrong — or, outside the bootstrap, missing — is put back
//! on the request it answers (see `answered`). The bootstrap messages —
//! `server/discover` and `initialize` — are handed back whole, as one JSON
//! answer, because the SDK's bootstrap reader skips an error it meets on a
//! stream and would never see a refusal.
//!
//! ## Dropping a stream hangs up its exchange
//!
//! Every response stream handed to the SDK owns its HTTP exchange: when the
//! SDK drops the stream, the connection under it is shut down at once, and the
//! thread blocked reading it ends. At MCP 2026-07-28 that is how a request is
//! cancelled — the SDK sends no `notifications/cancelled` there, and the server
//! takes the closed response stream as the cancellation — so a listen the SDK
//! replaces, or a request it cancels, stops at the server too rather than
//! holding a connection and a subscription until the socket's timeout. At
//! 2025-11-25 a closed stream cancels nothing, and the SDK still sends the
//! notification. A call its caller merely abandons is NOT hung up: the SDK
//! keeps that stream until it ends or the client closes (see
//! `RmcpClient::call_tool`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use serde_json::Value;
use sse_stream::{Error as SseError, Sse};

use crate::http::{Hangup, HttpError, HttpTransport};

/// What an extension adds to a connection's `subscriptions/listen`: members
/// of its filter, and the notifications it delivers on that stream, which go
/// to the host's notification queue rather than to the SDK. rmcp's filter is
/// a closed struct, and its subscription ends a listen on any notification
/// outside it — which would end the listen that carries everything else.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListenExtension {
    /// Members added to `params.notifications` of every listen.
    pub members: serde_json::Map<String, Value>,
    /// Notification methods taken off the listen stream for the host.
    pub methods: Vec<String>,
}

/// The SDK's transport, backed by agentd's authenticated HTTP.
#[derive(Clone)]
pub struct AgentdHttp {
    http: Arc<HttpTransport>,
    timeout: Duration,
    /// Why the last bootstrap request (`server/discover`, `initialize`) got no
    /// JSON-RPC answer at all, when it did not. The SDK sees only "the
    /// transport failed"; whether the server refused the request, never saw
    /// it, or went silent is what decides the fallback.
    bootstrap_failure: Arc<Mutex<Option<BootstrapFailure>>>,
    /// The `capabilities` object of the last bootstrap answer, exactly as the
    /// server sent it: the SDK's typed copy drops members it does not model.
    raw_capabilities: Arc<Mutex<Option<Value>>>,
    /// How long a `subscriptions/listen` stream may stay silent, when that is
    /// longer than `timeout`. A listen is quiet whenever nothing changes, so
    /// the connection's timeout — meant for a server that stopped answering —
    /// would cut every quiet one, and updates sent while it is re-opened are
    /// lost.
    listen_idle: Option<Duration>,
    /// How many listen streams this connection's idle bound has cut — read
    /// by the listen pump to tell an idle cut from any other end.
    idle_cuts: Arc<AtomicU64>,
    /// Every tool name a `tools/list` answer named, as the server sent it:
    /// what the SDK then hands back is what it kept, and the difference is
    /// what it dropped without a word.
    tools_seen: Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// The host's notification queue — the one the SDK's handler fills —
    /// when the connection has one: what a server streams ahead of a
    /// `tools/list` answer goes there directly, so the answer can still come
    /// back whole.
    notifications: Option<Arc<Mutex<Vec<crate::rpc::Notification>>>>,
    /// An extension's part of every listen (see [`ListenExtension`]), set
    /// once the server's capabilities are known.
    listen_extension: Arc<Mutex<Option<ListenExtension>>>,
}

impl AgentdHttp {
    pub fn new(http: Arc<HttpTransport>, timeout: Duration) -> AgentdHttp {
        AgentdHttp {
            http,
            timeout,
            bootstrap_failure: Arc::default(),
            raw_capabilities: Arc::default(),
            listen_idle: None,
            idle_cuts: Arc::default(),
            tools_seen: Arc::default(),
            notifications: None,
            listen_extension: Arc::default(),
        }
    }

    /// Add `extension`'s members to every listen on this connection, and take
    /// its notifications off the listen stream for the host's queue (which
    /// [`Self::with_notifications`] names). Read per listen, so it can be set
    /// once the server's capabilities are known.
    pub fn with_listen_extension(
        mut self,
        extension: Arc<Mutex<Option<ListenExtension>>>,
    ) -> AgentdHttp {
        self.listen_extension = extension;
        self
    }

    /// Queue what a server streams ahead of a `tools/list` answer onto
    /// `queue` — the handler's — rather than handing the answer back on a
    /// stream (see `post_message`).
    pub fn with_notifications(
        mut self,
        queue: Arc<Mutex<Vec<crate::rpc::Notification>>>,
    ) -> AgentdHttp {
        self.notifications = Some(queue);
        self
    }

    /// Take `v`, a message a server sent ahead of a `tools/list` answer, off
    /// the stream: a notification goes to the host's queue if it is one the
    /// handler would have queued (any other the handler would have ignored).
    /// `false` — not taken — for a request, which only the handler can
    /// answer, and on a connection with no queue.
    fn queue_ahead(&self, v: &Value) -> bool {
        match &self.notifications {
            Some(queue) => crate::rmcp_client::queue_as_handler(queue, v),
            None => false,
        }
    }

    /// Let a listen stream stay silent for `idle` — never less than the
    /// connection's timeout — before it is cut.
    pub fn with_listen_idle(mut self, idle: Duration) -> AgentdHttp {
        self.listen_idle = Some(idle.max(self.timeout));
        self
    }

    /// The count of listen streams cut for idleness; see `idle_cuts`.
    pub fn idle_cuts(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.idle_cuts)
    }

    /// The tool names the server's `tools/list` answers named; see
    /// `tools_seen`.
    pub fn tools_seen(&self) -> Arc<Mutex<std::collections::BTreeSet<String>>> {
        Arc::clone(&self.tools_seen)
    }

    /// See `bootstrap_failure`.
    pub(crate) fn bootstrap_failure(&self) -> Option<BootstrapFailure> {
        *self
            .bootstrap_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Record why a bootstrap exchange got no JSON-RPC answer.
    fn record(&self, failure: BootstrapFailure) {
        *self
            .bootstrap_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(failure);
    }

    /// See `raw_capabilities`.
    pub(crate) fn raw_capabilities(&self) -> Option<Value> {
        self.raw_capabilities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// How a bootstrap exchange failed when it carried no JSON-RPC answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapFailure {
    /// The server answered with this status and no JSON-RPC body.
    Status(u16),
    /// The request never reached a server: the TCP dial (or the name lookup)
    /// failed, this build cannot speak the endpoint's scheme, or the signer
    /// could not produce the credential it presents. A TLS
    /// failure is not this: the handshake runs on the first read, so it
    /// surfaces as `NoAnswer`, the fallback meets the same failure, and the
    /// error names both.
    Unreached,
    /// The server took the request and gave no answer: it went silent past
    /// the timeout, reset the connection, or answered with no body.
    NoAnswer,
}

impl BootstrapFailure {
    fn of(e: &HttpError) -> BootstrapFailure {
        match e {
            HttpError::Status(s) => BootstrapFailure::Status(*s),
            HttpError::Refused(c) => BootstrapFailure::Status(c.status),
            HttpError::RetryAfter {
                status, sent: true, ..
            } => BootstrapFailure::Status(*status),
            HttpError::Connect(_)
            | HttpError::Unsupported(_)
            | HttpError::Credential(_)
            | HttpError::RetryAfter { sent: false, .. } => BootstrapFailure::Unreached,
            HttpError::Http(_) | HttpError::NoResponse | HttpError::NoEventStream => {
                BootstrapFailure::NoAnswer
            }
        }
    }
}

/// Keep a successful bootstrap answer's `capabilities` as sent.
fn keep_capabilities(slot: &Mutex<Option<Value>>, answer: &Value) {
    if let Some(caps) = answer.get("result").and_then(|r| r.get("capabilities")) {
        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(caps.clone());
    }
}

/// Fill in what a `server/discover` answer may leave out and rmcp requires.
///
/// A workaround for rmcp's strictness, not a protocol choice: rmcp refuses a
/// DiscoverResult without `resultType`, `ttlMs` or `cacheScope`, while the
/// caching rules say a client SHOULD read an absent `ttlMs` as 0 (fresh for no
/// time at all). The values filled in are the most cautious ones: complete,
/// never cached, and scoped to this client.
fn complete_discover_answer(answer: &mut Value) {
    let Some(result) = answer.get_mut("result").and_then(Value::as_object_mut) else {
        return;
    };
    if !result.contains_key("supportedVersions") {
        return;
    }
    result
        .entry("resultType")
        .or_insert_with(|| Value::from("complete"));
    result.entry("ttlMs").or_insert_with(|| Value::from(0));
    result
        .entry("cacheScope")
        .or_insert_with(|| Value::from("private"));
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
    Done(Result<Option<Value>, HttpError>),
}

/// Hangs up an exchange when dropped. It lives in the state of every stream
/// handed to the SDK, so dropping the stream is what closes the connection —
/// see the module doc. Hanging up an exchange that already finished does
/// nothing.
struct HangupOnDrop(Arc<Hangup>);

impl Drop for HangupOnDrop {
    fn drop(&mut self) {
        self.0.hang_up();
    }
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
    /// **The bootstrap messages are handed back whole.** `server/discover` and
    /// `initialize` come back as one JSON answer, not a stream: the SDK reads
    /// them looking for a RESPONSE and passes over an error on a stream, so a
    /// server's refusal — the `-32601` of a server without `server/discover`,
    /// the `-32022` of one that does not speak the offered revision — would be
    /// skipped and the handshake would fail for want of an answer instead of
    /// with the server's reason. A failure with no JSON-RPC answer at all is
    /// the transport's error. An `initialize` whose server speaks first (a log
    /// line, a ping) keeps its stream, with the answer at its end.
    ///
    /// **A server's JSON-RPC answer is the request's answer**, whatever status
    /// or frame carried it (see [`HttpTransport::send`]), and is put back on
    /// the request by id (`answered`).
    ///
    /// **The stream is live, not a replay.** The spec lets a server interleave a
    /// REQUEST of its own on the response stream of a POST — an elicitation, a
    /// sampling call — and then block until the client answers it (over a
    /// separate POST) before finishing the original reply. Collecting the frames
    /// and returning them once the reply landed would deadlock exactly that
    /// exchange: the server waits for an answer the SDK has not been shown yet,
    /// the POST runs to its timeout, and the frames are dropped with the error.
    /// So the blocking read runs on its own thread and forwards each frame as it
    /// arrives.
    ///
    /// **A request does not hold the connection.** The SDK's worker runs up to
    /// 16 ordinary POSTs at once, but `initialize`, `server/discover` and
    /// `notifications/initialized` are ordering barriers it sends alone, and
    /// whatever this waits for on one of those, every later message on the
    /// connection waits for too. A full window holds the next POST the same
    /// way. So a request's stream is handed back at once, and a slow tool — or
    /// one abandoned at its caller's bound and left running to the socket's
    /// timeout — holds no call but its own. Only the bootstrap waits:
    /// `initialize` for its first frame, because the session it opens rides
    /// that answer's head, and `server/discover` for its answer, which nothing
    /// can be sent before anyway. A notification, or our answer to a server's
    /// request, waits for its `202`, which a server gives at once; the SDK
    /// bounds replies and cancellations (its control POSTs) at 5 s.
    async fn post_message(
        &self,
        _uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        _session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let mut body = serde_json::to_vec(&message)
            .map_err(|e| StreamableHttpError::Client(TransportError(e.to_string())))?;
        let request_id = request_id_of(&message);
        let (bootstrap, discovers) = match &message {
            rmcp::model::JsonRpcMessage::Request(r) => match r.request {
                rmcp::model::ClientRequest::DiscoverRequest(_) => (true, true),
                rmcp::model::ClientRequest::InitializeRequest(_) => (true, false),
                _ => (false, false),
            },
            _ => (false, false),
        };
        let lists_tools = matches!(
            &message,
            rmcp::model::JsonRpcMessage::Request(r)
                if matches!(r.request, rmcp::model::ClientRequest::ListToolsRequest(_))
        );
        let listen_idle = match &message {
            rmcp::model::JsonRpcMessage::Request(r) => match r.request {
                rmcp::model::ClientRequest::SubscriptionsListenRequest(_) => self.listen_idle,
                _ => None,
            },
            _ => None,
        };
        // An extension's part of a listen: its filter members in the body, and
        // its notifications diverted to the host as they arrive.
        let mut divert: Vec<String> = Vec::new();
        if listen_idle.is_some()
            && let Some(ext) = self
                .listen_extension
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            && self.notifications.is_some()
        {
            let mut v: Value = serde_json::from_slice(&body)
                .map_err(|e| StreamableHttpError::Client(TransportError(e.to_string())))?;
            if let Some(filter) = v
                .pointer_mut("/params/notifications")
                .and_then(Value::as_object_mut)
            {
                filter.extend(ext.members);
                body = serde_json::to_vec(&v)
                    .map_err(|e| StreamableHttpError::Client(TransportError(e.to_string())))?;
                divert = ext.methods;
            }
        }
        let diverted = (!divert.is_empty())
            .then(|| self.notifications.clone())
            .flatten();
        let http = Arc::clone(&self.http);
        let timeout = self.timeout;
        let extra = header_pairs(auth_header, custom_headers);
        if bootstrap {
            *self
                .bootstrap_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Pumped>();
        // Not awaited: the send owns a thread for as long as the server keeps
        // the exchange open, and this call must return before it finishes. The
        // thread ends when the send does — when the server finishes, or when
        // whoever holds the exchange drops it. A dropped receiver alone would
        // not end it: a send blocked reading a silent server notices nothing
        // until its next frame or the socket's timeout. So `hangup` goes with
        // the exchange, and dropping it shuts the connection down under the
        // read (`HangupOnDrop`).
        //
        // A plain thread, not the runtime's blocking pool: a call its caller
        // abandoned at its bound keeps its stream inside the SDK, so its
        // exchange runs on until the socket's own timeout, and the runtime
        // waits for its pool when the client is dropped — so an abandoned call
        // would hold whoever drops the client for up to that long.
        let hangup = Arc::new(listen_idle.map_or_else(Hangup::default, Hangup::with_stream_idle));
        let held = HangupOnDrop(Arc::clone(&hangup));
        let notes_tx = tx.clone();
        // A listen whose stream went quiet past its idle bound ended for that
        // reason alone; counted, so the pump can say so.
        let idle_cuts = listen_idle.map(|_| Arc::clone(&self.idle_cuts));
        let tools_seen = lists_tools.then(|| Arc::clone(&self.tools_seen));
        std::thread::spawn(move || {
            let refs: Vec<(&str, &str)> = extra
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let resp = http.send_until_hangup(
                request_id,
                &body,
                timeout,
                &refs,
                |n| {
                    if let Some(queue) = &diverted
                        && let Some(method) = n.get("method").and_then(Value::as_str)
                        && divert.iter().any(|m| m == method)
                    {
                        queue.lock().unwrap_or_else(|e| e.into_inner()).push(
                            crate::rpc::Notification::new(method, n.get("params").cloned()),
                        );
                        return;
                    }
                    let _ = notes_tx.send(Pumped::Message(n));
                },
                &hangup,
            );
            if let (Some(n), Err(HttpError::Http(io))) = (&idle_cuts, &resp)
                && hangup.streamed()
                && matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
            {
                n.fetch_add(1, Ordering::SeqCst);
            }
            if let (Some(seen), Ok(Some(v))) = (&tools_seen, &resp) {
                let names = v["result"]["tools"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t["name"].as_str().map(str::to_string));
                seen.lock().unwrap_or_else(|e| e.into_inner()).extend(names);
            }
            let _ = tx.send(Pumped::Done(resp));
        });

        // A request other than the bootstrap: its answer, or the error that
        // ended its exchange, reaches the SDK on the stream. An exchange that
        // fails before the server said anything becomes an error RESPONSE to
        // this request carrying the transport's reason — the SDK itself answers
        // a stream that closed early with a synthesised error, but a generic
        // one, and "HTTP 401" is what an operator needs to see.
        // A `tools/list` answer is handed back whole too, as the bootstrap's
        // is: rmcp records each tool's `x-mcp-header` schema from a list
        // answer — what a call's `Mcp-Param-*` headers are built from — only
        // as it takes the answer off the wire, and an answer it drains from a
        // stream that has just ended skips that step. A list answered on a
        // stream could then leave the next call without its headers (and the
        // tools rmcp would drop undropped), depending on timing. Whole, it is
        // always recorded.
        let whole = bootstrap || lists_tools;
        if let (Some(id), false) = (request_id, whole) {
            let session = self.http.session_id();
            // The exchange's hang-up rides in the stream's state, so the
            // stream dropped is the connection closed.
            let frames = futures::stream::unfold((Some(rx), held), move |(rx, held)| async move {
                let mut rx = rx?;
                match rx.recv().await {
                    Some(Pumped::Message(v)) => Some((Ok(as_event(&v)), (Some(rx), held))),
                    Some(Pumped::Done(Ok(Some(v)))) => {
                        Some((Ok(as_event(&answered(id, v, false))), (Some(rx), held)))
                    }
                    Some(Pumped::Done(Err(e))) => {
                        Some((Ok(as_event(&failed(id, &e.to_string()))), (None, held)))
                    }
                    // Accepted with no body, or the pump vanished: the stream
                    // is over, and the SDK fails the request for want of an
                    // answer.
                    Some(Pumped::Done(Ok(None))) | None => None,
                }
            });
            return Ok(StreamableHttpPostResponse::Sse(Box::pin(frames), session));
        }

        // Wait for the first frame. `Mcp-Session-Id` rides the response HEAD,
        // which the transport has already recorded by the time any frame can
        // reach us, so reading it here is not early.
        //
        // `server/discover` waits for its answer instead: nothing it is sent
        // before that has a lifecycle to arrive in yet. So does `tools/list`,
        // whose answer must come back whole: a notification sent ahead of it
        // goes to the host's queue directly. A request sent ahead of it cannot
        // wait — the handler answers it — so it, and the answer after it, go
        // on the stream.
        //
        // `held` is held across the wait, so a `post_message` future the SDK
        // drops while it waits hangs the exchange up too.
        let first = loop {
            match rx.recv().await {
                Some(Pumped::Message(_)) if discovers => continue,
                Some(Pumped::Message(v)) if lists_tools && self.queue_ahead(&v) => continue,
                first => break first,
            }
        };
        let first = match first {
            Some(Pumped::Message(v)) => v,
            // The whole answer, as one message: see this method's doc.
            Some(Pumped::Done(Ok(Some(v)))) => {
                let mut v = match request_id {
                    Some(id) => answered(id, v, bootstrap),
                    None => v,
                };
                if discovers {
                    complete_discover_answer(&mut v);
                }
                if bootstrap {
                    keep_capabilities(&self.raw_capabilities, &v);
                }
                let message =
                    serde_json::from_value(v).map_err(StreamableHttpError::Deserialize)?;
                return Ok(StreamableHttpPostResponse::Json(
                    message,
                    self.http.session_id(),
                ));
            }
            // A notification: nothing came back, and nothing should have.
            Some(Pumped::Done(Ok(None))) => return Ok(StreamableHttpPostResponse::Accepted),
            // The server no longer knows the session this message carried.
            // Said as exactly that, because it is the SDK's one signal for it;
            // the connection is built with `reinit_on_expired_session(false)`,
            // so the SDK hands it back rather than handshaking again behind the
            // host's back — and the host, which knows what the lost session
            // held, re-dials and re-subscribes.
            // A list whose exchange failed fails as any request does: an
            // error answer carrying the transport's reason (and a lost session
            // is told apart by the client, as for every request).
            Some(Pumped::Done(Err(e))) if lists_tools => {
                let id = request_id.unwrap_or_default();
                let message = serde_json::from_value(failed(id, &e.to_string()))
                    .map_err(StreamableHttpError::Deserialize)?;
                return Ok(StreamableHttpPostResponse::Json(
                    message,
                    self.http.session_id(),
                ));
            }
            Some(Pumped::Done(Err(_))) if self.http.session_lost() => {
                return Err(StreamableHttpError::SessionExpired);
            }
            Some(Pumped::Done(Err(e))) => {
                if bootstrap {
                    self.record(BootstrapFailure::of(&e));
                }
                return Err(StreamableHttpError::Client(TransportError(e.to_string())));
            }
            // The pump vanished without reporting — only reachable if the
            // blocking thread itself died, which is a dead socket either way.
            None => {
                if bootstrap {
                    self.record(BootstrapFailure::NoAnswer);
                }
                return Err(StreamableHttpError::Client(TransportError(
                    "mcp: response stream ended with no reply".into(),
                )));
            }
        };
        let session = self.http.session_id();

        // An `initialize` answered at the end of a stream: its capabilities
        // are kept as they arrive, like a whole answer's.
        let caps_slot = bootstrap.then(|| Arc::clone(&self.raw_capabilities));
        let rest = futures::stream::unfold((rx, held), move |(mut rx, held)| {
            let caps_slot = caps_slot.clone();
            async move {
                match rx.recv().await {
                    Some(Pumped::Message(v)) => Some((Ok(as_event(&v)), (rx, held))),
                    Some(Pumped::Done(Ok(Some(v)))) => {
                        let v = match request_id {
                            Some(id) => answered(id, v, bootstrap),
                            None => v,
                        };
                        if let Some(slot) = &caps_slot {
                            keep_capabilities(slot, &v);
                        }
                        Some((Ok(as_event(&v)), (rx, held)))
                    }
                    // A request's exchange that failed after its first frame:
                    // its answer is the transport's reason, as on any other
                    // request's stream, not the SDK's generic "closed early".
                    Some(Pumped::Done(Err(e))) if !bootstrap => {
                        request_id.map(|id| (Ok(as_event(&failed(id, &e.to_string()))), (rx, held)))
                    }
                    // Done — cleanly, or with an error the first frame already
                    // outlived. Either way this POST's stream is over, and ending
                    // the stream is how the SDK is told so.
                    _ => None,
                }
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
        _session_id: Arc<str>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<http::HeaderName, http::HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        // agentd's transport ends a session by dropping the connection; there is
        // no separate DELETE to make, and a server that keeps a session it will
        // never hear from again ages it out.
        Ok(())
    }

    /// Open the server→client event stream: the channel a server uses to send
    /// requests of its own (elicitation, sampling, roots) and unsolicited
    /// notifications.
    ///
    /// The dial carries what the SDK hands it — the negotiated
    /// `MCP-Protocol-Version`, and on a reconnect the `Last-Event-ID` to resume
    /// from — on top of the socket's own credentials and signature.
    ///
    /// The dial is made before this returns, so its outcome is the SDK's to
    /// act on: a server with no push channel (`405`, or an answer that is not
    /// an event stream) is reported as exactly that, and the SDK stops asking.
    /// A stream handed back unopened would instead look like one that opened
    /// and ended — which the SDK redials, every second, for the life of the
    /// connection.
    ///
    /// A `404` to a dial that carried the session is recorded on the socket
    /// ([`HttpTransport::session_lost`]) before it is handed back like any
    /// other failure: on an idle connection the stream's redial is the only
    /// request there is, so it is how the host learns the server forgot the
    /// session — and re-dials the connection, which ends this stream with it.
    ///
    /// Every other failure is handed back as a stream that ends at once. The
    /// SDK takes an error from the connection's FIRST dial as final — it never
    /// dials again — so a refused connect while the server restarts, a `503`,
    /// or a `401` just before a credential refresh would otherwise cost the
    /// connection its push channel for good, `resources/updated` wakes and
    /// server requests with it, while every call kept working. An ended
    /// stream is redialled after the SDK's retry interval instead.
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
        let mut extra = header_pairs(auth_header, custom_headers);
        if let Some(id) = last_event_id {
            extra.push(("Last-Event-ID".to_string(), id));
        }
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel::<Result<(), HttpError>>();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Sse, SseError>>();

        // A blocking reader pumping into a channel: the stream the SDK polls is
        // the receiving end. When the SDK drops the stream the next send fails
        // and the reader stops, so a closed stream closes the connection at the
        // server's next event or the socket's timeout. (This GET channel is
        // 2025-11-25's alone, where a closed stream cancels nothing, so it is
        // not hung up the way a POST's response stream is.) The
        // reader is not `Send`, so the thread that dials it is the one that
        // reads it, and it reports the dial's outcome first.
        std::thread::spawn(move || {
            let refs: Vec<(&str, &str)> = extra
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let mut events = match http.open_events(timeout, &refs) {
                Ok(events) => {
                    let _ = opened_tx.send(Ok(()));
                    events
                }
                Err(e) => {
                    let _ = opened_tx.send(Err(e));
                    return;
                }
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

        match opened_rx.await {
            Ok(Ok(())) => Ok(Box::pin(
                tokio_stream::wrappers::UnboundedReceiverStream::new(rx),
            )),
            Ok(Err(HttpError::Status(405) | HttpError::NoEventStream)) => {
                Err(StreamableHttpError::ServerDoesNotSupportSse)
            }
            // Possibly transient, so not a verdict on the server: an ended
            // stream, which the SDK redials.
            Ok(Err(_)) | Err(_) => Ok(Box::pin(futures::stream::empty())),
        }
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
    if !matches!(message, rmcp::model::JsonRpcMessage::Request(_)) {
        return None;
    }
    serde_json::to_value(message)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_i64))
}

/// A request's final answer, put back on the request it answers.
///
/// The SDK routes an answer by its id, and a server that refused a request it
/// could not read — a gateway's `400`, a version check ahead of the parser —
/// answers with whatever id it had: none, `null`, or a placeholder (the Python
/// SDK's middleware sends the string `"server-error"`). On this POST it can
/// only be this request's answer, so an error whose id is not `id` gets `id`.
///
/// A null or absent id is the exception during the bootstrap: the SDK's
/// bootstrap reader already takes an id-less error as the answer, so there it
/// is left as the server sent it. Everywhere else the SDK drops an id-less
/// error as unroutable, and the call would wait out the end of its stream.
/// A success is never touched — its id is the SDK's to check.
fn answered(id: i64, mut v: Value, bootstrap: bool) -> Value {
    if v.get("error").is_none() {
        return v;
    }
    let fix = match v.get("id") {
        None | Some(Value::Null) => !bootstrap,
        Some(got) => got.as_i64() != Some(id),
    };
    if fix && let Some(obj) = v.as_object_mut() {
        obj.insert("id".to_string(), Value::from(id));
    }
    v
}

/// The error response a request gets when its exchange ended before the
/// server answered it: JSON-RPC's internal error, with the transport's reason
/// as the message.
fn failed(id: i64, reason: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": crate::rpc::INTERNAL_ERROR, "message": format!("mcp: transport: {reason}")}
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    /// A server that answers one request with `reply` (or holds it, for
    /// `None`), reading the request first.
    fn serve_once(reply: Option<&'static str>) -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ep = format!("http://{}/mcp", l.local_addr().unwrap());
        std::thread::spawn(move || {
            let Ok((conn, _)) = l.accept() else { return };
            let mut w = conn.try_clone().unwrap();
            let mut r = BufReader::new(conn);
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let _ = r.read_exact(&mut vec![0u8; len]);
            match reply {
                Some(bytes) => {
                    let _ = w.write_all(bytes.as_bytes());
                }
                None => std::thread::sleep(Duration::from_secs(3)),
            }
        });
        ep
    }

    /// POST a `server/discover` and return what the socket recorded.
    fn recorded(ep: &str, timeout: Duration) -> Option<BootstrapFailure> {
        let http = Arc::new(HttpTransport::new(
            crate::http::McpEndpoint::parse(ep).unwrap(),
            vec![],
        ));
        let agentd = AgentdHttp::new(http, timeout);
        let discover: rmcp::model::ClientJsonRpcMessage = serde_json::from_value(
            serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "server/discover", "params": {}}),
        )
        .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(agentd.post_message(Arc::from(ep), discover, None, None, HashMap::new()))
            .ok();
        agentd.bootstrap_failure()
    }

    #[test]
    fn a_bootstrap_failure_is_recorded_by_kind() {
        use BootstrapFailure::*;
        let t = Duration::from_secs(5);
        let rpc_401 = "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 63\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":0,\"error\":{\"code\":-32001,\"message\":\"no\"}}";
        assert_eq!(
            recorded(&serve_once(Some(rpc_401)), t),
            Some(Status(401)),
            "an auth failure is its status, whatever its body"
        );
        for (reply, status) in [
            (
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
                401,
            ),
            ("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n", 403),
            ("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n", 404),
            (
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 4\r\n\r\ndown",
                503,
            ),
        ] {
            assert_eq!(recorded(&serve_once(Some(reply)), t), Some(Status(status)));
        }
        assert_eq!(
            recorded(
                &serve_once(Some("HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")),
                t
            ),
            Some(NoAnswer),
            "accepted and never answered"
        );
        assert_eq!(
            recorded(&serve_once(None), Duration::from_millis(300)),
            Some(NoAnswer),
            "silent past the timeout"
        );
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/mcp", l.local_addr().unwrap())
        };
        assert_eq!(recorded(&closed, t), Some(Unreached), "nothing listening");
        let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 145\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"resultType\":\"complete\",\"supportedVersions\":[\"2026-07-28\"],\"capabilities\":{},\"ttlMs\":0,\"cacheScope\":\"private\"}}";
        assert_eq!(recorded(&serve_once(Some(ok)), t), None, "answered");
    }
}

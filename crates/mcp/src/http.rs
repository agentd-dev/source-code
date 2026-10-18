// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP **Streamable HTTP** client transport.
//!
//! A conformant remote MCP server is reached by `POST`ing a JSON-RPC message to a
//! single endpoint; the server replies with either a `application/json` body (one
//! message) or a `text/event-stream` (SSE) carrying one or more messages. A
//! server-assigned `Mcp-Session-Id` (returned on `initialize`) is echoed on every
//! subsequent request. Server→client notifications ride an optional long-lived
//! `GET` SSE stream. Which headers the protocol needs beyond those — the
//! negotiated `MCP-Protocol-Version` among them — is the SDK's to say: it hands
//! them to [`HttpTransport::send`] per request.
//!
//! The transport reuses the hand-rolled [`net::http`] client: `https://` runs
//! over TCP+TLS (optionally mutual TLS), `http://` over plain TCP (a local
//! sidecar). Neither spawns a process: the transport has no local exec surface,
//! so a hostile server config cannot turn into command execution here.

use net::http::{self, SseEvent, Url};
#[cfg(feature = "tls")]
use net::tls::ClientIdentity;
use serde_json::Value;
use std::io;
use std::net::{Shutdown, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// What a host stamps into the `params._meta` of every request a socket
/// sends. Called once per request, so a W3C trace context (SEP-414:
/// `traceparent`, `tracestate`) gives each request a span of its own. The
/// keys go as a set: a request whose `_meta` already holds any of them — a
/// caller's own `traceparent` for one call — keeps its own and gets none of
/// these, so a `tracestate` never rides beside another context's
/// `traceparent`.
pub type RequestMeta = std::sync::Arc<dyn Fn() -> serde_json::Map<String, Value> + Send + Sync>;

/// A resolved MCP endpoint: where to connect + the HTTP `path`/`Host` to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpEndpoint {
    /// `https://host[:port]/path` (TCP + TLS) or `http://…` (plain TCP).
    Tcp {
        host: String,
        port: u16,
        tls: bool,
        path: String,
        host_header: String,
    },
}

impl McpEndpoint {
    /// Parse an MCP server's endpoint URL: `https://…` or `http://…`, with the
    /// server's path (e.g. `/mcp`).
    pub fn parse(s: &str) -> Result<McpEndpoint, String> {
        let url = Url::parse(s)?;
        Ok(McpEndpoint::Tcp {
            tls: url.is_tls(),
            host_header: url.host_header(),
            host: url.host,
            port: url.port,
            path: url.path,
        })
    }

    /// The transport scheme name for the manifest/logs (never the address/creds).
    pub fn scheme(&self) -> &'static str {
        match self {
            McpEndpoint::Tcp { tls: true, .. } => "https",
            McpEndpoint::Tcp { tls: false, .. } => "http",
        }
    }

    fn http_path(&self) -> &str {
        match self {
            McpEndpoint::Tcp { path, .. } => path,
        }
    }

    fn host_header(&self) -> &str {
        match self {
            McpEndpoint::Tcp { host_header, .. } => host_header,
        }
    }
}

/// An MCP transport error (connect / HTTP / protocol).
#[derive(Debug)]
pub enum HttpError {
    Connect(io::Error),
    Http(io::Error),
    /// A non-2xx HTTP status.
    Status(u16),
    /// The build lacks the feature this endpoint needs (e.g. `tls`).
    Unsupported(String),
    /// No JSON-RPC response matched the request id before the stream ended.
    NoResponse,
    /// The server answered the notification `GET` with something other than an
    /// event stream: it offers no push channel.
    NoEventStream,
    /// The request signer could not produce the credential the endpoint is
    /// configured to present ([`RequestSigner::sign`] failed), so the request
    /// was not sent. Carries the signer's cause.
    Credential(String),
    /// The server refused the request's credential (`401`) or what it may do
    /// (`403`) with a `WWW-Authenticate` challenge naming an error code —
    /// `invalid_token`, `insufficient_scope`, `sign_in_required` — after the
    /// signer, consulted, had nothing to retry with.
    Refused(Challenge),
    /// The server answered `429` or `503` with a `Retry-After`. Every request
    /// on this socket fails at once, unsent, until the wait is over: then
    /// `sent` is `false` and `wait` is what is left of it.
    RetryAfter {
        status: u16,
        wait: Duration,
        sent: bool,
    },
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Connect(e) => write!(f, "mcp-http: connect: {e}"),
            HttpError::Http(e) => write!(f, "mcp-http: {e}"),
            HttpError::Status(s) => write!(f, "mcp-http: server returned HTTP {s}"),
            HttpError::Unsupported(m) => write!(f, "mcp-http: {m}"),
            HttpError::NoResponse => write!(f, "mcp-http: no JSON-RPC response before stream end"),
            HttpError::NoEventStream => {
                write!(f, "mcp-http: server has no GET SSE notification stream")
            }
            HttpError::Credential(m) => write!(f, "mcp-http: not sent, no credential: {m}"),
            HttpError::Refused(c) => write!(f, "mcp-http: server returned HTTP {}: {c}", c.status),
            HttpError::RetryAfter {
                status,
                wait,
                sent: true,
            } => write!(
                f,
                "mcp-http: server returned HTTP {status} and asked for no request for {}s",
                wait.as_secs()
            ),
            HttpError::RetryAfter {
                status,
                wait,
                sent: false,
            } => write!(
                f,
                "mcp-http: not sent: the server asked for no request for {}s more (HTTP {status})",
                wait.as_secs().max(1)
            ),
        }
    }
}
impl std::error::Error for HttpError {}

/// One `WWW-Authenticate` challenge (RFC 9110 §11.6.1, the RFC 6750 §3
/// parameters): its scheme and the error code and scope it names. The
/// `error_description` is not kept: it is the server's prose, and a typed
/// error carries only what a caller can act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Challenge {
    pub status: u16,
    /// `Bearer`, `DPoP`, … as the server wrote it.
    pub scheme: String,
    pub error: Option<String>,
    /// The scopes the server says the request needs (`insufficient_scope`).
    pub scope: Option<String>,
}

impl Challenge {
    /// The challenge of a `WWW-Authenticate` value that names an `error`:
    /// the first such one when the header lists several, so a `DPoP` offer
    /// beside a `Bearer error=…` does not hide the error. `None` when none
    /// does — a bare `Bearer realm=…` asks for a credential and says nothing
    /// a retry could change.
    pub fn parse(status: u16, header: &str) -> Option<Challenge> {
        challenges(header)
            .into_iter()
            .map(|(scheme, params)| {
                let get = |k: &str| {
                    params
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(k))
                        .map(|(_, v)| v.clone())
                };
                Challenge {
                    status,
                    scheme,
                    error: get("error"),
                    scope: get("scope"),
                }
            })
            .find(|c| c.error.is_some())
    }
}

impl std::fmt::Display for Challenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error.as_deref().unwrap_or("refused"))?;
        if let Some(scope) = &self.scope {
            write!(f, " (needs scope \"{scope}\")")?;
        }
        Ok(())
    }
}

/// Split a `WWW-Authenticate` value into its challenges: a scheme token,
/// then `name=value` parameters (a token or a quoted string), commas between
/// them and between challenges. A `token68` credential is skipped.
fn challenges(header: &str) -> Vec<(String, Vec<(String, String)>)> {
    let b = header.as_bytes();
    let blank = |c: u8| matches!(c, b' ' | b'\t');
    let token_end = |mut i: usize| {
        while i < b.len() && !matches!(b[i], b' ' | b'\t' | b',' | b'=' | b'"') {
            i += 1;
        }
        i
    };
    let mut out: Vec<(String, Vec<(String, String)>)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if blank(b[i]) || b[i] == b',' {
            i += 1;
            continue;
        }
        let end = token_end(i);
        if end == i {
            // A stray `=` or quote: step over it.
            i += 1;
            continue;
        }
        let word = header[i..end].to_string();
        let mut j = end;
        while j < b.len() && blank(b[j]) {
            j += 1;
        }
        if b.get(j) != Some(&b'=') {
            // A bare token: the scheme of the next challenge.
            out.push((word, Vec::new()));
            i = end;
            continue;
        }
        if out.is_empty() || matches!(b.get(j + 1), Some(b'=') | None) {
            // `token68=`: a credential, not a parameter.
            i = j;
            while i < b.len() && b[i] == b'=' {
                i += 1;
            }
            continue;
        }
        // `name=value`: a parameter of the challenge in progress.
        i = j + 1;
        while i < b.len() && blank(b[i]) {
            i += 1;
        }
        let value = if b.get(i) == Some(&b'"') {
            i += 1;
            let mut v = Vec::new();
            while i < b.len() && b[i] != b'"' {
                if b[i] == b'\\' && i + 1 < b.len() {
                    i += 1;
                }
                v.push(b[i]);
                i += 1;
            }
            i += 1;
            String::from_utf8_lossy(&v).into_owned()
        } else {
            let end = token_end(i);
            let v = header[i..end].to_string();
            i = end;
            v
        };
        if let Some((_, params)) = out.last_mut() {
            params.push((word, value));
        }
    }
    out
}

/// The `Host` authority (host[:port]) of an MCP endpoint URL — the `@authority`
/// AAuth signs over. Best-effort (a parse failure yields an empty string).
pub fn authority_of(endpoint: &str) -> String {
    McpEndpoint::parse(endpoint)
        .map(|e| e.host_header().to_string())
        .unwrap_or_default()
}

/// Is `v` a JSON-RPC 2.0 error response — the version tag, an integer code
/// and a string message — rather than some other JSON error a gateway wrote?
fn is_json_rpc_error(v: &Value) -> bool {
    let error = v.get("error");
    v.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && error.and_then(|e| e.get("code")).is_some_and(Value::is_i64)
        && error
            .and_then(|e| e.get("message"))
            .is_some_and(Value::is_string)
}

/// A way to hang up one exchange from outside the thread that blocks on it.
///
/// The send reads its answer on a blocking socket, and the only thing that
/// stops a blocking read early is the socket going away under it. So each
/// connection the exchange opens is armed here — the raw TCP socket, beneath
/// any TLS — and [`Hangup::hang_up`] shuts it down: the read fails, the send
/// returns, its thread ends, and the server sees the connection close. At MCP
/// 2026-07-28 that close IS the cancellation of the request it carried.
#[derive(Debug, Default)]
pub(crate) struct Hangup {
    closed: AtomicBool,
    socket: Mutex<Option<TcpStream>>,
    /// How long the exchange's event stream may stay silent once its head
    /// has arrived, when that differs from the connection's timeout (which
    /// still bounds the dial, the request, the head and the first event).
    stream_idle: Option<std::time::Duration>,
    /// Set once the stream's idle bound applies: a read timeout from then on
    /// is the stream going quiet, not a server that never answered.
    streaming: AtomicBool,
}

impl Hangup {
    /// A handle whose exchange, once its event stream is open, may stay
    /// silent for `idle` rather than the connection's timeout: a listen,
    /// which is quiet whenever nothing changes.
    pub(crate) fn with_stream_idle(idle: std::time::Duration) -> Hangup {
        Hangup {
            stream_idle: Some(idle),
            ..Hangup::default()
        }
    }

    /// Did the stream's idle bound apply (see `streaming`)?
    pub(crate) fn streamed(&self) -> bool {
        self.streaming.load(Ordering::SeqCst)
    }

    /// Give the armed connection the stream's own idle bound, now that its
    /// first event (a listen's acknowledgment) has arrived: until then the
    /// connection's timeout bounds a server that holds the answer back. The
    /// socket is shared with whatever wraps it (TLS included), so its read
    /// timeout bounds every read the stream makes.
    fn open_stream(&self) {
        let Some(idle) = self.stream_idle else { return };
        self.streaming.store(true, Ordering::SeqCst);
        if let Some(s) = self
            .socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = s.set_read_timeout(Some(idle));
        }
    }

    /// Hang up: the armed connection is shut down now, and any connection the
    /// exchange opens later is shut down as it is armed. Safe at any point,
    /// after the exchange finished included.
    pub(crate) fn hang_up(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let socket = self.socket.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(s) = socket {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    /// Arm the connection an attempt is about to use. `false` (and the
    /// connection shut down) if the exchange was already hung up — between the
    /// dial and here, or before an AAuth retry. The check and the store happen
    /// under the lock [`Self::hang_up`] takes, so a hang-up racing this either
    /// sees the socket or is seen by it.
    fn arm(&self, s: TcpStream) -> bool {
        let mut slot = self.socket.lock().unwrap_or_else(|e| e.into_inner());
        if self.closed.load(Ordering::SeqCst) {
            let _ = s.shutdown(Shutdown::Both);
            return false;
        }
        *slot = Some(s);
        true
    }

    /// The exchange is over: let go of the armed socket without shutting it
    /// down, so the connection closes with the exchange rather than with
    /// whoever still holds this handle.
    fn disarm(&self) {
        self.socket.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// A request's answer: the server's JSON-RPC message, and the HTTP status
/// it came with — a JSON-RPC error a server sends with a `5xx` is an answer,
/// and one that may say the request ran.
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) message: Value,
    pub(crate) status: u16,
}

/// The classification of one [`HttpTransport::send_once`] attempt (AAuth loop).
enum SendOutcome {
    /// A final JSON-RPC answer (or `None` for a notification ack).
    Result(Option<Answer>),
    /// A terminal transport/HTTP error.
    Error(HttpError),
    /// The signer satisfied an `AAuth-Requirement`; re-sign and retry.
    RetryAuth,
}

/// The AAuth-relevant fields of a server response. Handed to
/// [`RequestSigner::on_response`] so the signer can satisfy a runtime
/// `AAuth-Requirement` and decide whether a retry would now succeed.
#[derive(Debug, Clone, Default)]
pub struct AuthResponse {
    pub status: u16,
    /// The `AAuth-Requirement` header value (e.g. `agent-token`,
    /// `auth-token; resource-token="…"`, `interaction; url=…; code=…`).
    pub requirement: Option<String>,
    /// An opaque `AAuth-Access` token the server issued (Case B).
    pub access: Option<String>,
    /// A `Location` for a pending interaction (202) to poll.
    pub location: Option<String>,
    /// A `Signature-Error` / `AAuth-Error` detail (diagnostics only).
    pub error: Option<String>,
    /// The `WWW-Authenticate` value: an RFC 6750 challenge (`error`,
    /// `scope`, `resource_metadata`) or an RFC 9449 `DPoP` one.
    pub www_authenticate: Option<String>,
    /// A `DPoP-Nonce` the server issued (RFC 9449 §8).
    pub dpop_nonce: Option<String>,
    /// How long the server asked the client to wait (`Retry-After`).
    pub retry_after: Option<Duration>,
}

/// A per-request signer: AAuth, or a credential the caller presents (a
/// bearer, a SigV4 signature). The transport calls [`sign`] just before each
/// dial (the returned `(name, value)` pairs become request headers — the
/// RFC 9421 `Signature-Input`/`Signature`/`Signature-Key`, an `Authorization`),
/// and [`on_response`] after, to react to the server's `AAuth-Requirement`
/// (adopt an access token, run the Person-Server flow) or its `401`/`403`
/// challenge (renew the credential it refused) and re-sign+retry.
/// Deliberately a trait, not an implementation: the crypto lives in the
/// caller, so this crate stays free of any crypto dependency.
pub trait RequestSigner: Send + Sync {
    /// Sign one request. `authority` is the `Host` value (host[:port]); `path`
    /// is the request-target. `body` is the JSON-RPC bytes (for a
    /// `content-digest` cover when the server requires it). Returns the
    /// headers to add: an empty vec sends the request unsigned on purpose (the
    /// server answers with its requirement). `Err` is a credential the signer
    /// cannot produce — none cached, expired with nothing to renew it, a
    /// source that does not answer — and the request is not sent: the
    /// transport fails it with [`HttpError::Credential`], carrying the cause.
    fn sign(
        &self,
        method: &str,
        authority: &str,
        path: &str,
        body: &[u8],
    ) -> Result<Vec<(String, String)>, String>;
    /// React to a response — every `401`, `403` and `202`, a `429` or `503`
    /// with a `Retry-After`, and any carrying an AAuth header: adopt an
    /// `AAuth-Access` token, satisfy an `AAuth-Requirement` (e.g. run the
    /// Person-Server exchange), renew a credential a `WWW-Authenticate`
    /// challenge refused, keep the wait a `Retry-After` asks for, and return
    /// `true` iff the request should be RE-SIGNED and retried (something was
    /// newly satisfied). The default reacts to nothing. May do network I/O
    /// (the PS token exchange, a token renewal).
    fn on_response(&self, _resp: &AuthResponse, _authority: &str) -> bool {
        false
    }
    /// An optional `AAuth-Capabilities` header value (interaction shapes the
    /// agent can drive). `None` = omit.
    fn capabilities(&self) -> Option<String> {
        None
    }
    /// Whether this server requires a `content-digest` cover (learned at
    /// discovery). The transport adds/covers the body digest when true.
    fn wants_content_digest(&self, _authority: &str) -> bool {
        false
    }
}

/// The Streamable HTTP transport for one MCP server. Cheap to hold; each request
/// opens a fresh connection (`Connection: close`), so there is no persistent
/// socket to reap. `session` is set from the `Mcp-Session-Id` on the answer to
/// an `initialize` request — the one message that opens a session — and echoed
/// thereafter. The header on any other answer is ignored: a server that stamps
/// one on a `server/discover` refusal must not have it carried into the
/// `initialize` that follows, and a revision without sessions never gets one
/// echoed back.
pub struct HttpTransport {
    endpoint: McpEndpoint,
    /// Caller-owned auth + framing headers (e.g. `Authorization`, `x-api-key`).
    /// Values may be secrets. They are never logged and never rendered into an
    /// error: this transport only writes them onto the wire.
    headers: Vec<(String, String)>,
    /// A client identity for mutual TLS (TCP+TLS endpoints only).
    #[cfg(feature = "tls")]
    identity: Option<ClientIdentity>,
    session: Mutex<Option<String>>,
    /// Set once the server answered `404` to a request that carried this
    /// socket's `Mcp-Session-Id`: Streamable HTTP's way of saying it no longer
    /// knows the session (a restart, an expiry). Every subscription the server
    /// held for it is gone, and only a new handshake gets a working session
    /// back — on a fresh socket, see [`Self::redial_copy`]. Never cleared: the
    /// session it describes never comes back.
    session_lost: AtomicBool,
    /// An optional per-request signer (AAuth, or a credential the caller
    /// presents). `None` = the endpoint is called unsigned (the default;
    /// static-header/mTLS auth is unaffected).
    signer: Option<std::sync::Arc<dyn RequestSigner>>,
    /// The host's `_meta` for every request; see [`RequestMeta`].
    request_meta: Option<RequestMeta>,
    /// The instant before which the server asked for no request (a `429` or
    /// `503` with `Retry-After`), and the status that asked.
    not_before: Mutex<Option<(std::time::Instant, u16)>>,
    /// Dial through the SSRF guard ([`net::ssrf::connect_vetted`]): the host
    /// resolved once and only a vetted address connected, loopback, private
    /// and link-local refused unless the value is `true`. `None` dials by
    /// name, as an operator-configured endpoint always has.
    vetted: Option<bool>,
}

impl HttpTransport {
    pub fn new(endpoint: McpEndpoint, headers: Vec<(String, String)>) -> Self {
        HttpTransport {
            endpoint,
            headers,
            #[cfg(feature = "tls")]
            identity: None,
            session: Mutex::new(None),
            session_lost: AtomicBool::new(false),
            signer: None,
            request_meta: None,
            not_before: Mutex::new(None),
            vetted: None,
        }
    }

    /// The same endpoint, headers, signer and mTLS identity with none of this
    /// socket's session: what a re-dial after a lost session handshakes on.
    /// A fresh socket rather than this one with its session cleared, so a
    /// straggler of the lost connection — its notification stream redialling,
    /// a call abandoned at its bound — can neither carry the new session nor
    /// mark it lost.
    pub fn redial_copy(&self) -> HttpTransport {
        HttpTransport {
            endpoint: self.endpoint.clone(),
            headers: self.headers.clone(),
            #[cfg(feature = "tls")]
            identity: self.identity.clone(),
            session: Mutex::new(None),
            session_lost: AtomicBool::new(false),
            signer: self.signer.clone(),
            request_meta: self.request_meta.clone(),
            // The wait is the server's, not the lost session's.
            not_before: Mutex::new(*self.not_before.lock().unwrap_or_else(|e| e.into_inner())),
            vetted: self.vetted,
        }
    }

    /// Has the server forgotten this socket's session? See `session_lost`.
    pub fn session_lost(&self) -> bool {
        self.session_lost.load(Ordering::SeqCst)
    }

    /// Record a `404` answered to a request that carried a session: the one
    /// status Streamable HTTP gives a session the server no longer holds. A
    /// `404` to a request with no session (the `initialize` itself, a server
    /// that never issued one) says nothing about a session.
    /// Drop the session a refused handshake left behind, before another
    /// startup on this socket: a server can stamp a session on an
    /// `initialize` it refused with a 200, and the next request — a
    /// `server/discover` re-probe — must not carry it.
    pub(crate) fn forget_session(&self) {
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.session_lost.store(false, Ordering::SeqCst);
    }

    fn note_status(&self, status: u16, carried_session: bool) {
        if status == 404 && carried_session {
            self.session_lost.store(true, Ordering::SeqCst);
        }
    }

    /// The wait still to run before this socket may send, as the error a
    /// request gets for asking during it: unsent.
    fn waiting(&self) -> Result<(), HttpError> {
        let mut slot = self.not_before.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, status)) = *slot {
            let now = std::time::Instant::now();
            if now < at {
                return Err(HttpError::RetryAfter {
                    status,
                    wait: at - now,
                    sent: false,
                });
            }
            *slot = None;
        }
        Ok(())
    }

    /// Keep a `429` or `503`'s `Retry-After` as this socket's wait, cut to
    /// [`http::RETRY_AFTER_MAX`], and return the error the request that drew
    /// it gets; `None` for any other answer.
    fn note_retry_after(&self, status: u16, retry_after: Option<Duration>) -> Option<HttpError> {
        let wait = retry_after.filter(|_| matches!(status, 429 | 503))?;
        let wait = wait.min(http::RETRY_AFTER_MAX);
        *self.not_before.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::time::Instant::now() + wait, status));
        Some(HttpError::RetryAfter {
            status,
            wait,
            sent: true,
        })
    }

    /// Install a per-request signer. Builder-style; call before use.
    pub fn with_signer(mut self, signer: Option<std::sync::Arc<dyn RequestSigner>>) -> Self {
        self.signer = signer;
        self
    }

    /// Stamp `meta` into every request this socket sends ([`RequestMeta`]).
    pub fn set_request_meta(&mut self, meta: Option<RequestMeta>) {
        self.request_meta = meta;
    }

    /// Dial every connection through the SSRF guard; `allow_private`
    /// admits loopback, private and link-local addresses.
    pub fn set_vetted(&mut self, allow_private: bool) {
        self.vetted = Some(allow_private);
    }

    /// The request as sent: `body` with the host's [`RequestMeta`] added to
    /// `params._meta`. `None` — `body` goes as it is — for a notification, a
    /// body that is not a JSON-RPC request object, a `_meta` that is not an
    /// object or already holds one of the keys, and a socket with no source.
    fn stamped(&self, request_id: Option<i64>, body: &[u8]) -> Option<Vec<u8>> {
        let source = self.request_meta.as_ref()?;
        request_id?;
        let mut v: Value = serde_json::from_slice(body).ok()?;
        let obj = v.as_object_mut()?;
        obj.get("method")?;
        let params = obj
            .entry("params")
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()?;
        let meta = params
            .entry("_meta")
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()?;
        let add = source();
        if add.keys().any(|k| meta.contains_key(k)) {
            return None;
        }
        meta.extend(add);
        serde_json::to_vec(&v).ok()
    }

    /// Attach a mutual-TLS client identity (used only for `https://` endpoints).
    #[cfg(feature = "tls")]
    pub fn set_identity(&mut self, identity: Option<ClientIdentity>) {
        self.identity = identity;
    }

    pub fn scheme(&self) -> &'static str {
        self.endpoint.scheme()
    }

    /// Open a fresh connection to the endpoint as a boxed byte stream, applying
    /// `timeout` as the connect + read/write bound (each request opens its own
    /// connection, so the per-call timeout governs the whole exchange). Also
    /// returns a handle on the raw TCP socket, taken before any TLS wrap: what
    /// a [`Hangup`] shuts down to end the exchange from another thread.
    fn connect(&self, timeout: Duration) -> Result<(Box<dyn http::Stream>, TcpStream), HttpError> {
        match &self.endpoint {
            McpEndpoint::Tcp {
                host, port, tls, ..
            } => {
                let tcp = match self.vetted {
                    Some(allow_private) => {
                        net::ssrf::connect_vetted(host, *port, timeout, allow_private)
                    }
                    None => http::connect_tcp(host, *port, timeout),
                }
                .map_err(HttpError::Connect)?;
                let raw = tcp.try_clone().map_err(HttpError::Connect)?;
                if *tls {
                    #[cfg(feature = "tls")]
                    {
                        let s = net::tls::connect(tcp, host, self.identity.as_ref())
                            .map_err(HttpError::Connect)?;
                        Ok((Box::new(s), raw))
                    }
                    #[cfg(not(feature = "tls"))]
                    {
                        Err(HttpError::Unsupported(
                            "https:// MCP requires building with --features tls".into(),
                        ))
                    }
                } else {
                    Ok((Box::new(tcp), raw))
                }
            }
        }
    }

    /// The signer's headers for ONE dial: the signature over
    /// `@method`/`@authority`/`@path` (over `body` too when the server requires a
    /// content-digest cover), plus the optional `AAuth-Capabilities` advert.
    /// Empty without a signer — the endpoint is then called unsigned. A signer
    /// that cannot produce its credential is [`HttpError::Credential`], taken
    /// before the dial opens, so nothing reaches the server.
    ///
    /// Every dial goes through here, not just the request POST: a signed server
    /// answers an unsigned dial with a challenge, and on the long-lived
    /// notification stream that failure is silent in the worst way — the daemon
    /// keeps answering requests and simply never wakes.
    fn auth_headers(&self, method: &str, body: &[u8]) -> Result<Vec<(String, String)>, HttpError> {
        match &self.signer {
            Some(s) => {
                let authority = self.endpoint.host_header();
                let mut sig = s
                    .sign(method, authority, self.endpoint.http_path(), body)
                    .map_err(HttpError::Credential)?;
                if let Some(caps) = s.capabilities() {
                    sig.push(("AAuth-Capabilities".into(), caps));
                }
                Ok(sig)
            }
            None => Ok(Vec::new()),
        }
    }

    /// POST one JSON-RPC message. For a REQUEST (`id` present), return the JSON-RPC
    /// response with the matching id — parsed from the `application/json` body or
    /// pumped out of the `text/event-stream` (queuing any interleaved
    /// notifications via `on_notification`). For a NOTIFICATION (`id` absent), the
    /// server replies `202 Accepted` with no body and `Ok(None)` is returned.
    ///
    /// The server's JSON-RPC answer is the request's answer whatever status it
    /// rides: a non-2xx `application/json` body carrying an `error` with an
    /// integer `code` is returned as the response, so the caller sees the
    /// server's own error rather than the bare status. `401` and `403` are the
    /// exception — an auth failure is never taken for an answer: one whose
    /// `WWW-Authenticate` names an error is [`HttpError::Refused`], and any
    /// other [`HttpError::Status`]. A `429` or `503` with `Retry-After` is
    /// [`HttpError::RetryAfter`], and every request on this socket waits it
    /// out. Any other non-2xx is [`HttpError::Status`], and a 2xx to a request
    /// with an empty body is [`HttpError::NoResponse`].
    ///
    /// Echoes `Mcp-Session-Id`, and adopts it only from the answer to an
    /// `initialize` request.
    pub fn send<F: FnMut(Value)>(
        &self,
        request_id: Option<i64>,
        body: &[u8],
        timeout: Duration,
        extra_headers: &[(&str, &str)],
        on_notification: F,
    ) -> Result<Option<Value>, HttpError> {
        // Nobody else holds this handle, so nothing hangs the exchange up.
        self.send_until_hangup(
            request_id,
            body,
            timeout,
            extra_headers,
            on_notification,
            &Hangup::default(),
        )
    }

    /// [`Self::send`], ended early when `hangup` is hung up: the connection
    /// in use is shut down, the blocked read fails, and the send returns an
    /// error. A hang-up before a connection is in use stops it from being
    /// used — the request is never sent, and the answer is
    /// [`HttpError::NoResponse`].
    pub(crate) fn send_until_hangup<F: FnMut(Value)>(
        &self,
        request_id: Option<i64>,
        body: &[u8],
        timeout: Duration,
        extra_headers: &[(&str, &str)],
        on_notification: F,
        hangup: &Hangup,
    ) -> Result<Option<Value>, HttpError> {
        self.exchange_until_hangup(
            request_id,
            body,
            timeout,
            extra_headers,
            on_notification,
            hangup,
        )
        .map(|a| a.map(|a| a.message))
    }

    /// [`Self::send_until_hangup`], with the HTTP status the answer came
    /// with.
    pub(crate) fn exchange_until_hangup<F: FnMut(Value)>(
        &self,
        request_id: Option<i64>,
        body: &[u8],
        timeout: Duration,
        extra_headers: &[(&str, &str)],
        mut on_notification: F,
        hangup: &Hangup,
    ) -> Result<Option<Answer>, HttpError> {
        // Stamped once: every AAuth retry sends, and signs, the same bytes.
        let stamped = self.stamped(request_id, body);
        let out = self.send_attempts(
            request_id,
            stamped.as_deref().unwrap_or(body),
            timeout,
            extra_headers,
            &mut on_notification,
            hangup,
        );
        hangup.disarm();
        out
    }

    /// The AAuth request loop behind [`Self::send_until_hangup`].
    fn send_attempts<F: FnMut(Value)>(
        &self,
        request_id: Option<i64>,
        body: &[u8],
        timeout: Duration,
        extra_headers: &[(&str, &str)],
        on_notification: &mut F,
        hangup: &Hangup,
    ) -> Result<Option<Answer>, HttpError> {
        // AAuth request loop: send signed; if the server answers
        // with an `AAuth-Requirement` the signer can satisfy (adopt an access
        // token, run the Person-Server exchange), re-sign and retry — bounded,
        // so a mis-satisfied requirement cannot spin. Without a signer this is
        // exactly one pass.
        const MAX_AUTH_ATTEMPTS: usize = 3;
        // Read once, not per attempt: the body is the same on every retry.
        let opens_session = request_id.is_some() && is_initialize(body);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send_once(
                request_id,
                opens_session,
                body,
                timeout,
                extra_headers,
                on_notification,
                hangup,
            )? {
                SendOutcome::Result(v) => return Ok(v),
                SendOutcome::Error(e) => return Err(e),
                SendOutcome::RetryAuth if attempt < MAX_AUTH_ATTEMPTS => continue,
                // Out of retries: re-send once more unsigned-of-retry to surface
                // the server's real error rather than looping.
                SendOutcome::RetryAuth => {
                    return match self.send_once(
                        request_id,
                        opens_session,
                        body,
                        timeout,
                        extra_headers,
                        on_notification,
                        hangup,
                    )? {
                        SendOutcome::Result(v) => Ok(v),
                        SendOutcome::Error(e) => Err(e),
                        SendOutcome::RetryAuth => Err(HttpError::NoResponse),
                    };
                }
            }
        }
    }

    /// One send attempt: build headers (+ AAuth signing), POST, and classify the
    /// response — a parsed result, a terminal error, or `RetryAuth` (the signer
    /// satisfied an `AAuth-Requirement`; the caller re-signs and retries).
    /// `opens_session`: this is an `initialize`, whose answer's `Mcp-Session-Id`
    /// is adopted. Every attempt arms its own connection on `hangup`, an AAuth
    /// re-sign included, so a hang-up reaches whichever one is live.
    #[allow(clippy::too_many_arguments)]
    fn send_once<F: FnMut(Value)>(
        &self,
        request_id: Option<i64>,
        opens_session: bool,
        body: &[u8],
        timeout: Duration,
        extra_headers: &[(&str, &str)],
        on_notification: &mut F,
        hangup: &Hangup,
    ) -> Result<SendOutcome, HttpError> {
        // A server that asked for a wait gets nothing until it is over.
        self.waiting()?;
        // Signed before the dial: a credential the signer cannot produce
        // fails the request here, unsent. The owned strings must outlive the
        // borrowed header slice, so `signed` stays in scope until it is sent.
        let signed = self.auth_headers("POST", body)?;
        let (mut stream, raw) = self.connect(timeout)?;
        if !hangup.arm(raw) {
            // Hung up before this connection carried anything: the caller no
            // longer wants an answer, so the request is not sent at all.
            return Ok(SendOutcome::Error(HttpError::NoResponse));
        }
        let mut headers: Vec<(&str, &str)> = vec![
            ("Content-Type", "application/json"),
            ("Accept", "application/json, text/event-stream"),
        ];
        let session = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(sid) = &session {
            headers.push(("Mcp-Session-Id", sid));
        }
        // Caller-supplied per-request headers: what the SDK says this request
        // needs, the negotiated `MCP-Protocol-Version` among them.
        for (k, v) in extra_headers {
            headers.push((k, v));
        }
        for (k, v) in &self.headers {
            headers.push((k.as_str(), v.as_str()));
        }
        for (k, v) in &signed {
            headers.push((k.as_str(), v.as_str()));
        }

        let resp = http::send_streaming(
            stream.as_mut(),
            self.endpoint.host_header(),
            "POST",
            self.endpoint.http_path(),
            &headers,
            body,
        )
        .map_err(HttpError::Http)?;

        // Adopt a server-assigned session id — from an `initialize` answer
        // that succeeded only, see the struct doc. A refused `initialize` (a
        // version the server does not speak, an auth challenge) opened
        // nothing, so its retry or fallback must not carry what it stamped.
        if let (true, true, Some(sid)) = (
            opens_session,
            resp.is_success(),
            resp.header("mcp-session-id"),
        ) {
            *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
        }

        // The signer's reaction: adopt an AAuth access token, satisfy a
        // requirement, renew a credential the server refused, keep a wait the
        // server asked for. `on_response` returns whether a retry would now
        // differ. Only consulted when a signer is present AND the response
        // carries an auth signal (an AAuth requirement, an access token, a
        // 401, 403 or 202) or a `Retry-After` on a 429 or 503 — a plain
        // success skips it.
        let www_authenticate = resp.header("www-authenticate").map(str::to_string);
        let retry_after = resp.retry_after();
        if let Some(signer) = &self.signer {
            let ar = AuthResponse {
                status: resp.status,
                requirement: resp.header("aauth-requirement").map(str::to_string),
                access: resp.header("aauth-access").map(str::to_string),
                location: resp.header("location").map(str::to_string),
                error: resp
                    .header("signature-error")
                    .or_else(|| resp.header("aauth-error"))
                    .map(str::to_string),
                www_authenticate: www_authenticate.clone(),
                dpop_nonce: resp.header("dpop-nonce").map(str::to_string),
                retry_after,
            };
            if ar.requirement.is_some()
                || ar.access.is_some()
                || matches!(resp.status, 401 | 403 | 202)
                || (matches!(resp.status, 429 | 503) && retry_after.is_some())
            {
                let authority = self.endpoint.host_header().to_string();
                if signer.on_response(&ar, &authority) {
                    return Ok(SendOutcome::RetryAuth);
                }
            }
        }

        if !resp.is_success() {
            // First, so a `404` on a session marks it lost whatever the body.
            let status = resp.status;
            self.note_status(status, session.is_some());
            let throttled = self.note_retry_after(status, retry_after);
            // A refusal that says why is the error a caller can act on.
            if matches!(status, 401 | 403)
                && let Some(c) = www_authenticate
                    .as_deref()
                    .and_then(|h| Challenge::parse(status, h))
            {
                return Ok(SendOutcome::Error(HttpError::Refused(c)));
            }
            // A server that refuses a request in JSON-RPC — a version it does
            // not speak, a header that does not match the body — says why in
            // the body, and that is the request's answer: the code is what a
            // caller acts on, and the message is what an operator needs, where
            // the status alone says only "HTTP 400". An auth failure never is:
            // whatever its body says, `401`/`403` is the signal the credential
            // machinery and the operator act on. Only a JSON-RPC error counts:
            // a gateway's own JSON error (`{"error": {"code": 503, …}}`, with
            // no `jsonrpc`) is not one, and its status is what the operator
            // needs to see.
            let answerable = request_id.is_some()
                && !matches!(status, 401 | 403)
                && resp
                    .content_type()
                    .is_some_and(|ct| ct.eq_ignore_ascii_case("application/json"));
            if answerable
                && let Ok(bytes) = resp.into_body()
                && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
                && is_json_rpc_error(&v)
            {
                return Ok(SendOutcome::Result(Some(Answer { message: v, status })));
            }
            return Ok(SendOutcome::Error(
                throttled.unwrap_or(HttpError::Status(status)),
            ));
        }

        // A notification POST is acknowledged with an empty body (often 202).
        if request_id.is_none() {
            return Ok(SendOutcome::Result(None));
        }

        let status = resp.status;
        if resp.is_event_stream() {
            let mut sse = resp.sse();
            let mut first = true;
            while let Some(ev) = sse.next_event().map_err(HttpError::Http)? {
                if std::mem::take(&mut first) {
                    hangup.open_stream();
                }
                if let Some(msg) = route_message(&ev, request_id, on_notification) {
                    return Ok(SendOutcome::Result(Some(Answer {
                        message: msg,
                        status,
                    })));
                }
            }
            Ok(SendOutcome::Error(HttpError::NoResponse))
        } else {
            let bytes = resp.into_body().map_err(HttpError::Http)?;
            // A request accepted with nothing to say — a `202` — was not
            // answered, and saying so beats a JSON parse error about an empty
            // body.
            if bytes.iter().all(u8::is_ascii_whitespace) {
                return Ok(SendOutcome::Error(HttpError::NoResponse));
            }
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|e| HttpError::Http(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            Ok(SendOutcome::Result(Some(Answer { message: v, status })))
        }
    }

    /// The session id the server assigned, if this connection has one.
    pub fn session_id(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Open the long-lived server→client notification stream: a `GET` that the
    /// server answers with `text/event-stream`, carrying JSON-RPC notifications
    /// (e.g. `resources/updated`). Returns an owning SSE reader. `read_timeout`
    /// bounds each read so the caller's loop can poll a stop flag between events
    /// (clean shutdown). Errors if the server has no push channel (non-2xx, or
    /// [`HttpError::NoEventStream`] for a non-SSE response) — the caller then runs
    /// without server-initiated pushes.
    ///
    /// `extra_headers` are what the SDK says this dial needs, exactly as on the
    /// POST path: the negotiated `MCP-Protocol-Version`, which Streamable HTTP
    /// requires on every request after `initialize`, and on a reconnect the
    /// `Last-Event-ID` the server resumes the stream from.
    pub fn open_events(
        &self,
        read_timeout: Duration,
        extra_headers: &[(&str, &str)],
    ) -> Result<EventStream, HttpError> {
        // The push channel is signed exactly like the request path — an
        // `auth:`-configured server rejects an unsigned GET, and losing this
        // stream costs the daemon its whole reactivity story. The
        // challenge/re-sign loop stays on the POST path: this dial only ever
        // happens post-initialize, so the signer has already satisfied whatever
        // the server required and signs from what it learned there. Signed
        // before the dial, as a POST is — and not dialled at all while the
        // server's `Retry-After` runs.
        self.waiting()?;
        let signed = self.auth_headers("GET", b"")?;
        // The raw handle goes unused: this stream ends when its reader stops.
        let (stream, _) = self.connect(read_timeout)?;
        let mut headers: Vec<(&str, &str)> = vec![("Accept", "text/event-stream")];
        let session = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(sid) = &session {
            headers.push(("Mcp-Session-Id", sid));
        }
        headers.extend_from_slice(extra_headers);
        for (k, v) in &self.headers {
            headers.push((k.as_str(), v.as_str()));
        }
        for (k, v) in &signed {
            headers.push((k.as_str(), v.as_str()));
        }
        let resp = http::send_streaming(
            stream,
            self.endpoint.host_header(),
            "GET",
            self.endpoint.http_path(),
            &headers,
            b"",
        )
        .map_err(HttpError::Http)?;
        if !resp.is_success() {
            self.note_status(resp.status, session.is_some());
            let throttled = self.note_retry_after(resp.status, resp.retry_after());
            return Err(throttled.unwrap_or(HttpError::Status(resp.status)));
        }
        if !resp.is_event_stream() {
            return Err(HttpError::NoEventStream);
        }
        Ok(resp.sse())
    }
}

/// An owning SSE reader over the notification `GET` stream (a boxed transport
/// stream, so it survives on the notification thread).
pub type EventStream = http::SseReader<std::io::BufReader<Box<dyn http::Stream>>>;

/// Is this JSON-RPC body an `initialize` request — the one message whose answer
/// opens a session?
fn is_initialize(body: &[u8]) -> bool {
    // Only the method is read; every other field is skipped unparsed.
    #[derive(serde::Deserialize)]
    struct Head {
        method: Option<String>,
    }
    serde_json::from_slice::<Head>(body).is_ok_and(|h| h.method.as_deref() == Some("initialize"))
}

/// Route one SSE event: if its `data` is the JSON-RPC response for `request_id`,
/// return it; a message without a matching id (a notification/other) is handed to
/// `on_notification` and `None` is returned so the caller keeps reading.
///
/// A JSON-RPC ERROR (an `error`, no `method`) answers this POST whatever its id
/// says. A POST's stream carries only its own exchange — what else the server
/// sends on it is a request or notification of its own, and those carry a
/// `method` — and a server that could not read the request's id answers with a
/// null one, or none. Handed on as a notification it would be dropped, and the
/// request would end with no answer at all instead of the server's error.
fn route_message<F: FnMut(Value)>(
    ev: &SseEvent,
    request_id: Option<i64>,
    on_notification: &mut F,
) -> Option<Value> {
    let v: Value = serde_json::from_str(&ev.data).ok()?;
    let id_matches =
        matches!((request_id, v.get("id").and_then(Value::as_i64)), (Some(a), Some(b)) if a == b);
    let is_error = v.get("error").is_some() && v.get("method").is_none();
    if id_matches || (request_id.is_some() && is_error) {
        Some(v)
    } else {
        on_notification(v);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_endpoint() {
        let e = McpEndpoint::parse("https://mcp.example.com/mcp").unwrap();
        assert_eq!(e.scheme(), "https");
        assert_eq!(e.http_path(), "/mcp");
        assert_eq!(e.host_header(), "mcp.example.com");
        let McpEndpoint::Tcp {
            host, port, tls, ..
        } = e;
        assert_eq!(host, "mcp.example.com");
        assert_eq!(port, 443);
        assert!(tls);
    }

    #[test]
    fn parse_http_endpoint() {
        let e = McpEndpoint::parse("http://localhost:8080/mcp").unwrap();
        assert_eq!(e.scheme(), "http");
        assert_eq!(e.host_header(), "localhost:8080");
        assert_eq!(e.http_path(), "/mcp");
    }

    #[test]
    fn parse_rejects_bad_endpoints() {
        assert!(McpEndpoint::parse("ftp://x/").is_err());
        assert!(McpEndpoint::parse("not-a-url").is_err());
    }

    /// The RFC 6750 §3 examples, a `DPoP` offer beside a `Bearer` error, and
    /// a header that names no error.
    #[test]
    fn a_challenge_names_its_error_and_scope() {
        let c = Challenge::parse(
            403,
            r#"Bearer error="insufficient_scope", scope="files:read files:write", resource_metadata="https://rs/.well-known/oauth-protected-resource""#,
        )
        .expect("an error is named");
        assert_eq!(c.scheme, "Bearer");
        assert_eq!(c.error.as_deref(), Some("insufficient_scope"));
        assert_eq!(c.scope.as_deref(), Some("files:read files:write"));
        assert_eq!(
            c.to_string(),
            r#"insufficient_scope (needs scope "files:read files:write")"#
        );
        let c = Challenge::parse(
            401,
            r#"DPoP algs="ES256 PS256", Bearer realm="x", error="invalid_token", error_description="The access token expired""#,
        )
        .expect("the Bearer challenge's error");
        assert_eq!(
            (c.scheme.as_str(), c.error.as_deref()),
            ("Bearer", Some("invalid_token"))
        );
        assert_eq!(c.to_string(), "invalid_token");
        // A token68 credential, a bare token value, an escaped quote.
        let c = Challenge::parse(
            403,
            r#"Negotiate abc==, Bearer error=sign_in_required, scope="x \"y\" z""#,
        )
        .unwrap();
        assert_eq!(c.error.as_deref(), Some("sign_in_required"));
        assert_eq!(c.scope.as_deref(), Some(r#"x "y" z"#));
        assert_eq!(Challenge::parse(401, r#"Bearer realm="example""#), None);
        assert_eq!(Challenge::parse(401, ""), None);
    }

    #[test]
    fn route_message_matches_response_id_and_queues_notifications() {
        let mut notes: Vec<Value> = Vec::new();
        // A notification (no id) is queued, returns None.
        let n = SseEvent {
            data: r#"{"jsonrpc":"2.0","method":"notifications/message","params":{}}"#.into(),
            ..Default::default()
        };
        assert!(route_message(&n, Some(1), &mut |v| notes.push(v)).is_none());
        assert_eq!(notes.len(), 1);
        // The matching-id response is returned.
        let r = SseEvent {
            data: r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#.into(),
            ..Default::default()
        };
        let got = route_message(&r, Some(1), &mut |v| notes.push(v)).expect("response");
        assert_eq!(got["result"]["ok"], true);
        assert_eq!(notes.len(), 1, "response is not queued as a notification");
    }

    #[test]
    fn an_exchange_hung_up_before_it_is_sent_sends_nothing() {
        // The caller let go between the dial and the write — or before an
        // AAuth retry dials again. The connection is shut down unused: the
        // server sees it close with no request on it, and the send says it
        // got no answer.
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let ep = format!("http://{}/mcp", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut got = Vec::new();
            let _ = conn.read_to_end(&mut got);
            got
        });
        let t = HttpTransport::new(McpEndpoint::parse(&ep).unwrap(), vec![]);
        let hangup = Hangup::default();
        hangup.hang_up();
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let out = t.send_until_hangup(Some(1), body, Duration::from_secs(5), &[], |_| {}, &hangup);
        assert!(matches!(out, Err(HttpError::NoResponse)), "{out:?}");
        let got = server.join().unwrap();
        assert!(got.is_empty(), "sent: {}", String::from_utf8_lossy(&got));
    }
}

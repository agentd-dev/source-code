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
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
        }
    }
}
impl std::error::Error for HttpError {}

/// The `Host` authority (host[:port]) of an MCP endpoint URL — the `@authority`
/// AAuth signs over. Best-effort (a parse failure yields an empty string).
pub fn authority_of(endpoint: &str) -> String {
    McpEndpoint::parse(endpoint)
        .map(|e| e.host_header().to_string())
        .unwrap_or_default()
}

/// The classification of one [`HttpTransport::send_once`] attempt (AAuth loop).
enum SendOutcome {
    /// A final JSON-RPC result (or `None` for a notification ack).
    Result(Option<Value>),
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
}

/// A per-request AAuth signer. The transport calls [`sign`] just before each
/// POST (the returned `(name, value)` pairs become request headers — the
/// RFC 9421 `Signature-Input`/`Signature`/`Signature-Key`), and [`on_response`]
/// after, to react to the server's `AAuth-Requirement` (adopt an access token,
/// run the Person-Server flow) and re-sign+retry. Deliberately a trait, not an
/// implementation: the crypto lives in the caller, so this crate stays free of
/// any crypto dependency.
pub trait RequestSigner: Send + Sync {
    /// Sign one request. `authority` is the `Host` value (host[:port]); `path`
    /// is the request-target. `body` is the JSON-RPC bytes (for a
    /// `content-digest` cover when the server requires it). Returns headers to
    /// add; an empty vec = send unsigned (let the server answer with its
    /// requirement).
    fn sign(&self, method: &str, authority: &str, path: &str, body: &[u8])
    -> Vec<(String, String)>;
    /// React to a response: adopt an `AAuth-Access` token, satisfy
    /// an `AAuth-Requirement` (e.g. run the Person-Server exchange), and return
    /// `true` iff the request should be RE-SIGNED and retried (a requirement was
    /// newly satisfied). The default reacts to nothing. May do network I/O
    /// (the PS token exchange).
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
/// socket to reap. `session` is set from the server's `Mcp-Session-Id` on the
/// first response and echoed thereafter.
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
    /// An optional per-request AAuth signer. `None` = the endpoint is called
    /// unsigned (the default; static-bearer/mTLS auth is unaffected).
    signer: Option<std::sync::Arc<dyn RequestSigner>>,
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
    fn note_status(&self, status: u16, carried_session: bool) {
        if status == 404 && carried_session {
            self.session_lost.store(true, Ordering::SeqCst);
        }
    }

    /// Install a per-request signer (AAuth). Builder-style; call before use.
    pub fn with_signer(mut self, signer: Option<std::sync::Arc<dyn RequestSigner>>) -> Self {
        self.signer = signer;
        self
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
    /// connection, so the per-call timeout governs the whole exchange).
    fn connect(&self, timeout: Duration) -> Result<Box<dyn http::Stream>, HttpError> {
        match &self.endpoint {
            McpEndpoint::Tcp {
                host, port, tls, ..
            } => {
                let tcp = http::connect_tcp(host, *port, timeout).map_err(HttpError::Connect)?;
                if *tls {
                    #[cfg(feature = "tls")]
                    {
                        let s = net::tls::connect(tcp, host, self.identity.as_ref())
                            .map_err(HttpError::Connect)?;
                        Ok(Box::new(s))
                    }
                    #[cfg(not(feature = "tls"))]
                    {
                        Err(HttpError::Unsupported(
                            "https:// MCP requires building with --features tls".into(),
                        ))
                    }
                } else {
                    Ok(Box::new(tcp))
                }
            }
        }
    }

    /// The AAuth headers for ONE dial: the signature over
    /// `@method`/`@authority`/`@path` (over `body` too when the server requires a
    /// content-digest cover), plus the optional `AAuth-Capabilities` advert.
    /// Empty without a signer — the endpoint is then called unsigned.
    ///
    /// Every dial goes through here, not just the request POST: a signed server
    /// answers an unsigned dial with a challenge, and on the long-lived
    /// notification stream that failure is silent in the worst way — the daemon
    /// keeps answering requests and simply never wakes.
    fn auth_headers(&self, method: &str, body: &[u8]) -> Vec<(String, String)> {
        match &self.signer {
            Some(s) => {
                let authority = self.endpoint.host_header();
                let mut sig = s.sign(method, authority, self.endpoint.http_path(), body);
                if let Some(caps) = s.capabilities() {
                    sig.push(("AAuth-Capabilities".into(), caps));
                }
                sig
            }
            None => Vec::new(),
        }
    }

    /// POST one JSON-RPC message. For a REQUEST (`id` present), return the JSON-RPC
    /// response with the matching id — parsed from the `application/json` body or
    /// pumped out of the `text/event-stream` (queuing any interleaved
    /// notifications via `on_notification`). For a NOTIFICATION (`id` absent), the
    /// server replies `202 Accepted` with no body and `Ok(None)` is returned.
    /// Captures/echoes `Mcp-Session-Id`.
    pub fn send<F: FnMut(Value)>(
        &self,
        request_id: Option<i64>,
        body: &[u8],
        timeout: Duration,
        extra_headers: &[(&str, &str)],
        mut on_notification: F,
    ) -> Result<Option<Value>, HttpError> {
        // AAuth request loop: send signed; if the server answers
        // with an `AAuth-Requirement` the signer can satisfy (adopt an access
        // token, run the Person-Server exchange), re-sign and retry — bounded,
        // so a mis-satisfied requirement cannot spin. Without a signer this is
        // exactly one pass.
        const MAX_AUTH_ATTEMPTS: usize = 3;
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.send_once(
                request_id,
                body,
                timeout,
                extra_headers,
                &mut on_notification,
            )? {
                SendOutcome::Result(v) => return Ok(v),
                SendOutcome::Error(e) => return Err(e),
                SendOutcome::RetryAuth if attempt < MAX_AUTH_ATTEMPTS => continue,
                // Out of retries: re-send once more unsigned-of-retry to surface
                // the server's real error rather than looping.
                SendOutcome::RetryAuth => {
                    return match self.send_once(
                        request_id,
                        body,
                        timeout,
                        extra_headers,
                        &mut on_notification,
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
    fn send_once<F: FnMut(Value)>(
        &self,
        request_id: Option<i64>,
        body: &[u8],
        timeout: Duration,
        extra_headers: &[(&str, &str)],
        on_notification: &mut F,
    ) -> Result<SendOutcome, HttpError> {
        let mut stream = self.connect(timeout)?;
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
        // AAuth request signing. The owned strings must outlive the borrowed
        // header slice, so `signed` stays in scope until the request is sent.
        let signed = self.auth_headers("POST", body);
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

        // Adopt a server-assigned session id (initialize response).
        if let Some(sid) = resp.header("mcp-session-id") {
            *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(sid.to_string());
        }

        // AAuth response reaction: let the signer adopt an access
        // token / satisfy a requirement. `on_response` returns whether a retry
        // would now differ. Only consulted when a signer is present AND the
        // response carries an AAuth signal (a requirement, an access token, or a
        // 401/202) — a plain success skips it.
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
            };
            if ar.requirement.is_some()
                || ar.access.is_some()
                || resp.status == 401
                || resp.status == 202
            {
                let authority = self.endpoint.host_header().to_string();
                if signer.on_response(&ar, &authority) {
                    return Ok(SendOutcome::RetryAuth);
                }
            }
        }

        if !resp.is_success() {
            self.note_status(resp.status, session.is_some());
            return Ok(SendOutcome::Error(HttpError::Status(resp.status)));
        }

        // A notification POST is acknowledged with an empty body (often 202).
        if request_id.is_none() {
            return Ok(SendOutcome::Result(None));
        }

        if resp.is_event_stream() {
            let mut sse = resp.sse();
            while let Some(ev) = sse.next_event().map_err(HttpError::Http)? {
                if let Some(msg) = route_message(&ev, request_id, on_notification) {
                    return Ok(SendOutcome::Result(Some(msg)));
                }
            }
            Ok(SendOutcome::Error(HttpError::NoResponse))
        } else {
            let bytes = resp.into_body().map_err(HttpError::Http)?;
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|e| HttpError::Http(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            Ok(SendOutcome::Result(Some(v)))
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
        let stream = self.connect(read_timeout)?;
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
        // The push channel is signed exactly like the request path — an
        // `auth:`-configured server rejects an unsigned GET, and losing this
        // stream costs the daemon its whole reactivity story. The
        // challenge/re-sign loop stays on the POST path: this dial only ever
        // happens post-initialize, so the signer has already satisfied whatever
        // the server required and signs from what it learned there.
        let signed = self.auth_headers("GET", b"");
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
            return Err(HttpError::Status(resp.status));
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

/// Route one SSE event: if its `data` is the JSON-RPC response for `request_id`,
/// return it; a message without a matching id (a notification/other) is handed to
/// `on_notification` and `None` is returned so the caller keeps reading.
fn route_message<F: FnMut(Value)>(
    ev: &SseEvent,
    request_id: Option<i64>,
    on_notification: &mut F,
) -> Option<Value> {
    let v: Value = serde_json::from_str(&ev.data).ok()?;
    let id_matches =
        matches!((request_id, v.get("id").and_then(Value::as_i64)), (Some(a), Some(b)) if a == b);
    if id_matches {
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
}

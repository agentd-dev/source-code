// SPDX-License-Identifier: AGPL-3.0-only
//! The **Streamable HTTP** MCP server: HTTP/1.1 + SSE over TCP (plain, or TLS via
//! the [`net::tls`] acceptor), reusing the same [`Handler`] / [`lifecycle_response`]
//! / [`SubRegistry`](crate::server::SubRegistry) as the socket servers. This is the
//! serving mirror of the crate's HTTP *client* ([`crate::http`]) and the transport
//! the HTTPS control plane rides.
//!
//! Model (Streamable HTTP, both eras):
//!   * **Unary** — one `POST` carrying a JSON-RPC request; the reply is
//!     `application/json`. `initialize` is stamped with an `Mcp-Session-Id`
//!     (legacy). One request per connection (`Connection: close`), matching the
//!     client's dialer.
//!   * **Reactive** — a `POST subscriptions/listen` (modern, stateless): the
//!     connection becomes a long-lived `text/event-stream`. Each requested uri is
//!     run through the handler's normal `resources/subscribe` gate (so the
//!     embedder's per-origin subscribability rules apply unchanged) and, if
//!     accepted, this connection's SSE write half is registered in the shared
//!     registry — so the embedder's existing `notify_*` pushes reach it as SSE
//!     `data:` events. The stream is held open with periodic keep-alive comments;
//!     a failed write prunes the subscriptions and ends the connection.
//!
//! **Trust is never transport-derived.** Every request is classified by an
//! [`HttpAuth`] the embedder supplies (mutual-TLS client identity primary, bearer
//! token alternative); an unauthenticated peer gets `401` and never reaches the
//! handler.

use crate::rpc::{Incoming, Request};
use crate::server::{Handler, PeerOrigin, ServeStream, SharedWriter, SubRegistry};
use crate::wire::method;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How often an idle SSE stream writes a keep-alive comment (also the disconnect
/// probe — a failed write ends the stream and prunes its subscriptions).
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

/// Cap on a request body (JSON-RPC frames are small; this bounds a hostile peer).
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Cap on the whole request HEAD — request line plus every header line. A body
/// is bounded by its declared `Content-Length`; a head is bounded by nothing the
/// peer tells us, so it has to be bounded by us: without this, one connection
/// that never sends a newline grows a `String` until the process dies, and it
/// can do that BEFORE authenticating (the head is read to find the credential).
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Cap on the number of header lines kept. The byte cap already bounds the
/// total, but 64 KiB of four-byte headers is still ~13k `Vec` entries and every
/// `RequestParts::header` lookup is a linear scan over them — so bound the count
/// as well. Real callers send a handful.
const MAX_HEADERS: usize = 100;

/// A verified mTLS peer's identity, surfaced so an embedder can match a caller
/// to a named principal rather than merely observing "a cert was presented".
/// All-empty for a plain / no-client-cert connection. rustls has already verified
/// the chain; these fields are only *read* from the leaf certificate, so they are
/// safe to compare against — but only because verification already happened.
#[derive(Default, Clone)]
pub struct PeerId {
    /// A verified client certificate was presented (mutual TLS).
    pub cert: bool,
    /// The leaf certificate's subject CN, if any.
    pub subject: Option<String>,
    /// The leaf certificate's SANs (DNS / URI / IP); a SPIFFE X.509-SVID's
    /// `spiffe://…` arrives here as a URI SAN.
    pub sans: Vec<String>,
}

/// The parts of an inbound request an [`HttpAuth`] classifies trust from.
pub struct RequestParts<'a> {
    /// The request's headers (lowercased names), e.g. to read `authorization`.
    pub headers: &'a [(String, String)],
    /// Whether the peer presented a verified client certificate (mutual TLS).
    pub peer_cert: bool,
    /// The verified mTLS leaf subject CN, if any.
    pub peer_subject: Option<&'a str>,
    /// The verified mTLS leaf SANs (DNS / URI / IP); empty without a client cert.
    pub peer_sans: &'a [String],
}

impl RequestParts<'_> {
    /// The value of header `name` (compare lowercased), if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The embedder's auth policy: classify an inbound request's trust origin, or
/// reject it. Called once per connection before the handler sees anything. The
/// framework NEVER trusts by transport alone — return `None` to answer `401`.
pub trait HttpAuth: Send + Sync + 'static {
    fn authenticate(&self, parts: &RequestParts) -> Option<PeerOrigin>;
}

/// Allow every request as [`PeerOrigin::Management`] — for loopback dev / tests
/// only. NOT for a real listener (it makes the transport the trust boundary,
/// exactly the posture the pivot removes).
pub struct AllowAll;
impl HttpAuth for AllowAll {
    fn authenticate(&self, _parts: &RequestParts) -> Option<PeerOrigin> {
        Some(PeerOrigin::Management)
    }
}

/// Per-listener serving options beyond the handler/auth pair.
#[derive(Default, Clone)]
pub struct ServeOptions {
    /// Extra allowed browser `Origin` values (`scheme://host[:port]`, exact
    /// match) beyond the always-allowed loopback origins. A request from an
    /// allowed origin is served **with CORS response headers** (and its
    /// `OPTIONS` preflight answered), so a browser client on that origin can
    /// actually read the reply; any other cross-site origin stays 403
    /// (DNS-rebinding defense).
    pub extra_origins: Vec<String>,
}

/// How accepted TCP connections are wrapped: plaintext (loopback dev) or TLS
/// (the production control plane). The TLS variant carries the [`net::tls`]
/// acceptor, which drives the handshake (and, under mTLS, verifies the client
/// certificate) at accept time.
pub enum HttpAcceptor {
    /// Plaintext HTTP — loopback dev / tests only.
    Plain,
    /// HTTPS via a configured TLS acceptor (optionally mutual-TLS).
    #[cfg(feature = "tls")]
    Tls(net::tls::TlsAcceptor),
}

/// Bind a TCP listener for HTTP serving. Kept separate from the accept loop so
/// the caller can log/act on a successful bind (or propagate the error) before
/// the accept thread starts.
pub fn bind_tcp(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr)
}

/// Spawn the background accept thread: one blocking thread per connection, each
/// serving HTTP/1.1 (+ SSE) against `handler`, with trust classified by `auth`.
/// Peers that authenticate arrive in whatever [`PeerOrigin`] `auth` mints.
#[allow(clippy::too_many_arguments)]
pub fn spawn_accept_http(
    listener: TcpListener,
    acceptor: Arc<HttpAcceptor>,
    handler: Arc<dyn Handler>,
    auth: Arc<dyn HttpAuth>,
    subs: SubRegistry,
    conn_counter: Arc<AtomicU64>,
    write_timeout: Duration,
) -> io::Result<()> {
    spawn_accept_http_opts(
        listener,
        acceptor,
        handler,
        auth,
        subs,
        conn_counter,
        write_timeout,
        ServeOptions::default(),
    )
}

/// [`spawn_accept_http`] with explicit [`ServeOptions`] (extra browser origins).
#[allow(clippy::too_many_arguments)]
pub fn spawn_accept_http_opts(
    listener: TcpListener,
    acceptor: Arc<HttpAcceptor>,
    handler: Arc<dyn Handler>,
    auth: Arc<dyn HttpAuth>,
    subs: SubRegistry,
    conn_counter: Arc<AtomicU64>,
    write_timeout: Duration,
    opts: ServeOptions,
) -> io::Result<()> {
    let opts = Arc::new(opts);
    thread::Builder::new()
        .name("serve-http".into())
        .spawn(move || {
            for tcp in listener.incoming().flatten() {
                let acceptor = Arc::clone(&acceptor);
                let handler = Arc::clone(&handler);
                let auth = Arc::clone(&auth);
                let subs = Arc::clone(&subs);
                let conn_counter = Arc::clone(&conn_counter);
                let opts = Arc::clone(&opts);
                thread::Builder::new()
                    .name("serve-http-conn".into())
                    .spawn(move || {
                        accept_and_serve(
                            tcp,
                            &acceptor,
                            &handler,
                            &auth,
                            &subs,
                            &conn_counter,
                            write_timeout,
                            &opts,
                        );
                    })
                    .ok();
            }
        })
        .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn accept_and_serve(
    tcp: TcpStream,
    acceptor: &HttpAcceptor,
    handler: &Arc<dyn Handler>,
    auth: &Arc<dyn HttpAuth>,
    subs: &SubRegistry,
    conn_counter: &AtomicU64,
    write_timeout: Duration,
    opts: &ServeOptions,
) {
    let _ = tcp.set_write_timeout(Some(write_timeout));
    let _ = tcp.set_read_timeout(Some(write_timeout));
    match acceptor {
        HttpAcceptor::Plain => {
            serve_conn(
                tcp,
                PeerId::default(),
                handler,
                auth,
                subs,
                conn_counter,
                opts,
            );
        }
        // A failed TLS/mTLS handshake never reaches the protocol layer.
        #[cfg(feature = "tls")]
        HttpAcceptor::Tls(tls) => {
            if let Ok(stream) = tls.accept(tcp) {
                let peer = peer_id(&stream);
                serve_conn(stream, peer, handler, auth, subs, conn_counter, opts);
            }
        }
    }
}

/// Lift the verified mTLS peer's identity (subject CN + SANs) so the embedder's
/// auth policy can match it to a principal. `default()` when no client cert was
/// presented — an absent identity must read as "unidentified", never as a match.
#[cfg(feature = "tls")]
fn peer_id(stream: &net::tls::ServerTlsStream) -> PeerId {
    match net::tls::peer_identity(stream) {
        Some(id) => PeerId {
            cert: true,
            subject: id.subject_cn,
            sans: id.sans,
        },
        None => PeerId::default(),
    }
}

/// Serve one accepted (already TLS-terminated) connection. Generic over the
/// concrete stream so plain TCP and the TLS stream share one code path.
fn serve_conn<S: Read + Write + Send + 'static>(
    stream: S,
    peer: PeerId,
    handler: &Arc<dyn Handler>,
    auth: &Arc<dyn HttpAuth>,
    subs: &SubRegistry,
    conn_counter: &AtomicU64,
    opts: &ServeOptions,
) {
    let mut reader = BufReader::new(stream);
    let req = match read_request(&mut reader) {
        Ok(req) => req,
        // A refused head is answered, not just dropped: 431 is a real answer a
        // client can act on, and it costs nothing — the peer never got past the
        // reader, so no handler and no auth decision was involved.
        Err(ReadError::HeadTooLarge) => {
            let _ = write_simple(
                reader.get_mut(),
                431,
                "Request Header Fields Too Large",
                b"request head exceeds the header size/count limits",
                None,
            );
            return;
        }
        Err(ReadError::Incomplete) => return, // malformed / EOF before a full request
    };

    // DNS-rebinding defense, which Streamable HTTP requires: a browser
    // always sends `Origin`, so a page tricked into POSTing to a local agentd
    // carries its own site there. Reject any request whose `Origin` is present and
    // NOT a loopback origin (or a configured `ServeOptions::extra_origins` entry) —
    // a non-browser control-plane / mesh caller sends no `Origin` and is
    // unaffected. This is a transport-level guard, applied BEFORE auth (a rebind
    // presents no credential either, but defense-in-depth covers the loopback
    // `AllowAll` dev path where auth alone would let it through). An ALLOWED
    // browser origin is echoed back as CORS headers so the page can read replies.
    let cors = match check_origin(&req.headers, &opts.extra_origins) {
        OriginCheck::NoBrowser => None,
        OriginCheck::Allowed(o) => Some(o),
        OriginCheck::Denied => {
            let _ = write_simple(
                reader.get_mut(),
                403,
                "Forbidden",
                b"cross-origin request rejected",
                None,
            );
            return;
        }
    };
    // A CORS preflight (`OPTIONS`) carries no credential and never reaches the
    // handler — answer it before auth so a browser on an allowed origin can
    // proceed to the real POST.
    if req.method.eq_ignore_ascii_case("OPTIONS") {
        let _ = write_preflight(reader.get_mut(), cors.as_deref());
        return;
    }

    // Trust classification — the transport is never the boundary.
    let origin = {
        let parts = RequestParts {
            headers: &req.headers,
            peer_cert: peer.cert,
            peer_subject: peer.subject.as_deref(),
            peer_sans: &peer.sans,
        };
        auth.authenticate(&parts)
    };
    let Some(origin) = origin else {
        let _ = write_simple(reader.get_mut(), 401, "Unauthorized", b"", cors.as_deref());
        return;
    };

    // Only POST carries JSON-RPC; a GET (the legacy notification stream) is not
    // served — our clients negotiate the modern `subscriptions/listen` path.
    if !req.method.eq_ignore_ascii_case("POST") {
        let _ = write_simple(
            reader.get_mut(),
            405,
            "Method Not Allowed",
            b"POST a JSON-RPC request, or POST subscriptions/listen for the SSE stream",
            cors.as_deref(),
        );
        return;
    }

    let conn = conn_counter.fetch_add(1, Ordering::Relaxed);
    handler.on_connect(origin, conn);

    let incoming: Result<Incoming, _> = serde_json::from_slice(&req.body);
    match incoming {
        Ok(Incoming::Request(rpc_req)) if rpc_req.method == method::SUBSCRIPTIONS_LISTEN => {
            serve_listen(
                reader,
                rpc_req,
                origin,
                conn,
                handler,
                subs,
                cors.as_deref(),
            );
        }
        // A server-streaming method (the embedder declares them — e.g. the A2A
        // streaming pair): the response is an SSE stream of JSON-RPC frames.
        Ok(Incoming::Request(rpc_req)) if handler.streams(&rpc_req.method) => {
            serve_stream(reader, rpc_req, origin, conn, handler, cors.as_deref());
            remove_and_disconnect(subs, conn, origin, handler);
        }
        Ok(Incoming::Request(rpc_req)) => {
            serve_unary(
                reader.get_mut(),
                rpc_req,
                origin,
                conn,
                handler,
                cors.as_deref(),
            );
            remove_and_disconnect(subs, conn, origin, handler);
        }
        // A notification POST (e.g. notifications/initialized) → 202, no body.
        Ok(Incoming::Notification(_)) | Ok(Incoming::Response(_)) => {
            let _ = write_simple(reader.get_mut(), 202, "Accepted", b"", cors.as_deref());
            remove_and_disconnect(subs, conn, origin, handler);
        }
        Err(_) => {
            let _ = write_simple(
                reader.get_mut(),
                400,
                "Bad Request",
                b"invalid JSON-RPC frame",
                cors.as_deref(),
            );
            remove_and_disconnect(subs, conn, origin, handler);
        }
    }
}

/// A server-streaming request → a `text/event-stream` of JSON-RPC frames: the
/// dispatch's INTERMEDIATE frames flow through the shared SSE writer as `data:`
/// events while it runs, keep-alive comments cover the quiet stretches (the
/// dispatch may block for minutes between frames), and the RETURNED `Response`
/// is written as the FINAL event before the connection closes.
fn serve_stream<S: Read + Write + Send + 'static>(
    reader: BufReader<S>,
    req: Request,
    origin: PeerOrigin,
    conn: u64,
    handler: &Arc<dyn Handler>,
    cors: Option<&str>,
) {
    let mut stream = reader.into_inner();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n{}Connection: close\r\n\r\n",
        cors_headers(cors)
    );
    if stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.flush())
        .is_err()
    {
        return;
    }
    let writer: SharedWriter = Arc::new(Mutex::new(ServeStream::Http(Box::new(stream))));

    // Keep-alives while the dispatch blocks between frames — the same probe
    // cadence the listen stream uses. The mutex serializes them against frames.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ka = {
        let writer = Arc::clone(&writer);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                thread::sleep(SSE_KEEPALIVE);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let alive = writer
                    .lock()
                    .map(|mut w| {
                        w.write_all(b": keep-alive\n\n")
                            .and_then(|_| w.flush())
                            .is_ok()
                    })
                    .unwrap_or(false);
                if !alive {
                    break;
                }
            }
        })
    };

    let resp = handler.dispatch(req, origin, &writer, conn);
    stop.store(true, Ordering::Relaxed);
    if let Ok(mut w) = writer.lock() {
        let _ = w.write_response(&resp);
    }
    let _ = ka.join();
}

/// A unary request → `application/json` reply. Streaming responses (a2a) are a
/// later phase; the crate's dispatch returns one `Response` here.
fn serve_unary<S: Write>(
    stream: &mut S,
    req: Request,
    origin: PeerOrigin,
    conn: u64,
    handler: &Arc<dyn Handler>,
    cors: Option<&str>,
) {
    // A null sink for the dispatch's `writer` arg: unary methods don't push, and
    // a stray write must never corrupt the HTTP response.
    let sink: SharedWriter = Arc::new(Mutex::new(ServeStream::Http(Box::new(io::sink()))));
    let is_initialize = req.method == method::INITIALIZE;
    let resp = handler.dispatch(req, origin, &sink, conn);
    let body = serde_json::to_vec(&resp).unwrap_or_default();
    let session = if is_initialize {
        format!("Mcp-Session-Id: {}\r\n", next_session_id())
    } else {
        String::new()
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        cors_headers(cors),
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

/// A `subscriptions/listen` → the connection becomes a long-lived SSE stream.
/// Each requested uri is gated through the handler's normal `resources/subscribe`
/// path (so the embedder's per-origin rules apply); accepted ones register this
/// connection's SSE writer in the shared registry. The stream is then held open
/// with keep-alive comments until the peer disconnects.
fn serve_listen<S: Read + Write + Send + 'static>(
    reader: BufReader<S>,
    req: Request,
    origin: PeerOrigin,
    conn: u64,
    handler: &Arc<dyn Handler>,
    subs: &SubRegistry,
    cors: Option<&str>,
) {
    let uris = listen_uris(&req);
    let mut stream = reader.into_inner();
    // SSE response head.
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n{}Connection: close\r\n\r\n",
        cors_headers(cors)
    );
    if stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.flush())
        .is_err()
    {
        remove_and_disconnect(subs, conn, origin, handler);
        return;
    }

    // The connection's write half becomes the shared SSE sink. Registration goes
    // through the handler's own subscribe gate (a synthetic resources/subscribe
    // per uri), so this reuses the embedder's subscribability rules verbatim.
    let writer: SharedWriter = Arc::new(Mutex::new(ServeStream::Http(Box::new(stream))));
    for uri in &uris {
        let sub_req = Request::new(0, method::RESOURCES_SUBSCRIBE, Some(json!({ "uri": uri })));
        let _ = handler.dispatch(sub_req, origin, &writer, conn);
    }

    // Hold the stream open, using keep-alive comments as the disconnect probe.
    loop {
        thread::sleep(SSE_KEEPALIVE);
        let alive = writer
            .lock()
            .map(|mut w| {
                w.write_all(b": keep-alive\n\n")
                    .and_then(|_| w.flush())
                    .is_ok()
            })
            .unwrap_or(false);
        if !alive {
            break;
        }
    }
    remove_and_disconnect(subs, conn, origin, handler);
}

/// The `resourceSubscriptions` uri list from a `subscriptions/listen` request
/// (`params.notifications.resourceSubscriptions`).
fn listen_uris(req: &Request) -> Vec<String> {
    req.params
        .as_ref()
        .and_then(|p| p.get("notifications"))
        .and_then(|n| n.get("resourceSubscriptions"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn remove_and_disconnect(
    subs: &SubRegistry,
    conn: u64,
    origin: PeerOrigin,
    handler: &Arc<dyn Handler>,
) {
    crate::server::remove_conn_subscriptions(subs, conn);
    handler.on_disconnect(origin, conn);
}

/// A minimal status-only HTTP response (no JSON-RPC body).
fn write_simple<S: Write>(
    stream: &mut S,
    code: u16,
    reason: &str,
    body: &[u8],
    cors: Option<&str>,
) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
        cors_headers(cors),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// The CORS response headers for an allowed browser origin (empty otherwise).
/// The origin is echoed (never `*`) so a credentialed fetch works, and
/// `Mcp-Session-Id` is exposed for the initialize handshake.
fn cors_headers(origin: Option<&str>) -> String {
    match origin {
        Some(o) => format!(
            "Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\nAccess-Control-Expose-Headers: Mcp-Session-Id\r\n"
        ),
        None => String::new(),
    }
}

/// Answer a CORS preflight (`OPTIONS`). An allowed origin gets the grant; a
/// non-browser `OPTIONS` (no `Origin`) gets a plain 204.
fn write_preflight<S: Write>(stream: &mut S, origin: Option<&str>) -> io::Result<()> {
    let grant = match origin {
        Some(o) => format!(
            "Access-Control-Allow-Origin: {o}\r\nVary: Origin\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nAccess-Control-Allow-Headers: content-type, authorization, last-event-id, mcp-session-id\r\nAccess-Control-Max-Age: 600\r\n"
        ),
        None => String::new(),
    };
    let head =
        format!("HTTP/1.1 204 No Content\r\n{grant}Content-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

// ---- raw-HTTP surface (non-JSON-RPC embedders, e.g. the webhook listener) ----

/// A raw inbound HTTP request handed straight to a [`RawHandler`]: method, target
/// (path + optional query), lowercased headers, and the raw body. Unlike the
/// [`Handler`] path this does no JSON-RPC parsing and no transport-level auth —
/// the embedder routes by [`RawRequest::path`] and authenticates itself (e.g. a
/// per-webhook HMAC over the raw body). The DNS-rebind `Origin` guard and TLS
/// termination still apply.
pub struct RawRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Whether the peer presented a verified client certificate (mutual TLS).
    pub peer_cert: bool,
    /// The verified mTLS leaf subject CN, if any.
    pub peer_subject: Option<String>,
    /// The verified mTLS leaf SANs (DNS / URI / IP); empty without a client cert.
    pub peer_sans: Vec<String>,
}

impl RawRequest {
    /// Header `name` (compare lowercased), if present.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    /// The path portion of the target (any `?query` dropped).
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }
}

/// A raw HTTP response a [`RawHandler`] returns.
pub struct RawResponse {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    /// Extra response headers (e.g. `Retry-After` on a 429). Names as written.
    pub headers: Vec<(&'static str, String)>,
}

impl RawResponse {
    /// A JSON response.
    pub fn json(status: u16, reason: &'static str, body: impl Into<Vec<u8>>) -> RawResponse {
        RawResponse {
            status,
            reason,
            content_type: "application/json",
            body: body.into(),
            headers: Vec::new(),
        }
    }
    /// A short text response.
    pub fn text(status: u16, reason: &'static str, body: impl Into<Vec<u8>>) -> RawResponse {
        RawResponse {
            status,
            reason,
            content_type: "text/plain",
            body: body.into(),
            headers: Vec::new(),
        }
    }
}

/// A raw-HTTP embedder surface (the agentd webhook listener). One call per
/// request; the embedder routes and authenticates itself.
pub trait RawHandler: Send + Sync + 'static {
    fn handle(&self, req: &RawRequest) -> RawResponse;
}

/// Spawn a raw-HTTP accept loop — TLS-terminated like [`spawn_accept_http`], with
/// the same DNS-rebind `Origin` guard — dispatching each request to `handler`.
pub fn spawn_accept_raw(
    listener: TcpListener,
    acceptor: Arc<HttpAcceptor>,
    handler: Arc<dyn RawHandler>,
    write_timeout: Duration,
) -> io::Result<()> {
    thread::Builder::new()
        .name("serve-webhook".into())
        .spawn(move || {
            for tcp in listener.incoming().flatten() {
                let acceptor = Arc::clone(&acceptor);
                let handler = Arc::clone(&handler);
                thread::Builder::new()
                    .name("webhook-conn".into())
                    .spawn(move || {
                        let _ = tcp.set_write_timeout(Some(write_timeout));
                        let _ = tcp.set_read_timeout(Some(write_timeout));
                        match &*acceptor {
                            HttpAcceptor::Plain => serve_conn_raw(tcp, PeerId::default(), &handler),
                            #[cfg(feature = "tls")]
                            HttpAcceptor::Tls(tls) => {
                                if let Ok(stream) = tls.accept(tcp) {
                                    let peer = peer_id(&stream);
                                    serve_conn_raw(stream, peer, &handler);
                                }
                            }
                        }
                    })
                    .ok();
            }
        })
        .map(|_| ())
}

fn serve_conn_raw<S: Read + Write + Send + 'static>(
    stream: S,
    peer: PeerId,
    handler: &Arc<dyn RawHandler>,
) {
    let mut reader = BufReader::new(stream);
    let req = match read_request(&mut reader) {
        Ok(req) => req,
        Err(ReadError::HeadTooLarge) => {
            let _ = write_simple(
                reader.get_mut(),
                431,
                "Request Header Fields Too Large",
                b"request head exceeds the header size/count limits",
                None,
            );
            return;
        }
        Err(ReadError::Incomplete) => return,
    };
    // Webhook callers are servers, not browsers — loopback-only origins here.
    if matches!(check_origin(&req.headers, &[]), OriginCheck::Denied) {
        let _ = write_simple(
            reader.get_mut(),
            403,
            "Forbidden",
            b"cross-origin request rejected",
            None,
        );
        return;
    }
    let raw = RawRequest {
        method: req.method,
        target: req.target,
        headers: req.headers,
        body: req.body,
        peer_cert: peer.cert,
        peer_subject: peer.subject,
        peer_sans: peer.sans,
    };
    let resp = handler.handle(&raw);
    let _ = write_raw(reader.get_mut(), &resp);
}

fn write_raw<S: Write>(stream: &mut S, resp: &RawResponse) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        resp.reason,
        resp.content_type,
        resp.body.len()
    );
    for (name, value) in &resp.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

/// A parsed HTTP request: method, target, headers (lowercased names), body.
struct HttpRequest {
    method: String,
    #[allow(dead_code)]
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// The DNS-rebinding gate's verdict on a request's `Origin` header.
enum OriginCheck {
    /// No `Origin` header — a non-browser caller; no CORS needed.
    NoBrowser,
    /// An acceptable browser origin (loopback, or configured) — echo it as CORS.
    Allowed(String),
    /// A cross-site browser origin — reject 403.
    Denied,
}

/// Classify a request's `Origin` (if any) — the DNS-rebinding gate. No `Origin`
/// header → a non-browser caller, allowed with no CORS. Present → it must name a
/// loopback origin or an exact `extra` entry (a configured web-UI origin).
fn check_origin(headers: &[(String, String)], extra: &[String]) -> OriginCheck {
    match headers.iter().find(|(k, _)| k == "origin") {
        None => OriginCheck::NoBrowser,
        Some((_, origin)) => {
            if origin_is_loopback(origin) || extra.iter().any(|e| e == origin) {
                OriginCheck::Allowed(origin.clone())
            } else {
                OriginCheck::Denied
            }
        }
    }
}

/// Whether an `Origin` value (`scheme://host[:port]`) names a loopback host. The
/// opaque `"null"` origin (sandboxed iframe / `file://`) is treated as untrusted.
fn origin_is_loopback(origin: &str) -> bool {
    let after_scheme = origin.split_once("://").map(|(_, r)| r).unwrap_or(origin);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    // Strip the optional port, keeping a bracketed IPv6 literal intact.
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().unwrap_or(v6)
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// A process-unique `Mcp-Session-Id` for `initialize`. It is a correlation
/// HANDLE, not a credential (auth is mTLS/bearer, orthogonal), and each
/// connection is a single `Connection: close` request — so uniqueness, not
/// unguessability, is what matters. Time-millis + a monotone counter guarantees
/// uniqueness with no `rand` dependency (the minimalism moat).
fn next_session_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("s-{millis:x}-{n:x}")
}

/// Why a request could not be read.
enum ReadError {
    /// EOF before a complete request, a malformed head, or an over-long body —
    /// nothing worth answering; the connection is dropped.
    Incomplete,
    /// The head blew [`MAX_HEAD_BYTES`] / [`MAX_HEADERS`]. Answered `431` so the
    /// peer learns why, rather than being cut off mid-sentence.
    HeadTooLarge,
}

/// Read one head line (request line or header), spending from `budget`. The
/// budget bounds the READ itself rather than being checked after the fact: a
/// line is refused before it is buffered, which is the whole point — a peer that
/// never sends a newline must not be able to make us allocate for it. `Ok(0)`
/// is EOF.
fn read_head_line<S: Read>(
    reader: &mut BufReader<S>,
    budget: &mut usize,
    line: &mut String,
) -> Result<usize, ReadError> {
    // `budget + 1`: a line that exactly fills the budget still terminates inside
    // it, and one byte more is what proves the cap was blown.
    let n = Read::take(&mut *reader, *budget as u64 + 1)
        .read_line(line)
        .map_err(|_| ReadError::Incomplete)?;
    if n > *budget {
        return Err(ReadError::HeadTooLarge);
    }
    *budget -= n;
    Ok(n)
}

/// Read one HTTP/1.1 request (request line, headers, `Content-Length` body)
/// under the head bounds above.
fn read_request<S: Read>(reader: &mut BufReader<S>) -> Result<HttpRequest, ReadError> {
    let mut budget = MAX_HEAD_BYTES;
    let mut request_line = String::new();
    if read_head_line(reader, &mut budget, &mut request_line)? == 0 {
        return Err(ReadError::Incomplete);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or(ReadError::Incomplete)?.to_string();
    let target = parts.next().ok_or(ReadError::Incomplete)?.to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if read_head_line(reader, &mut budget, &mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((k, v)) = line.split_once(':') {
            if headers.len() >= MAX_HEADERS {
                return Err(ReadError::HeadTooLarge);
            }
            let name = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    if content_length > MAX_BODY {
        return Err(ReadError::Incomplete);
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|_| ReadError::Incomplete)?;
    }
    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{self, Response};
    use crate::server::{notify_resource_updated_keep, register_subscriber};
    use std::io::BufRead;

    /// A handler that advertises one subscribable resource and answers a tool
    /// call — enough to exercise unary + reactive over HTTP. Holds the SAME
    /// registry the server pushes through (the subscribe gate registers into it).
    struct TestHandler {
        subs: SubRegistry,
    }
    impl Handler for TestHandler {
        fn dispatch(
            &self,
            req: Request,
            _origin: PeerOrigin,
            writer: &SharedWriter,
            conn: u64,
        ) -> Response {
            if let Some(resp) = crate::server::lifecycle_response(
                &req,
                &json!({"name": "test", "version": "1"}),
                &json!({"tools": {}, "resources": {"subscribe": true}}),
            ) {
                return resp;
            }
            match req.method.as_str() {
                "tools/call" => Response::ok(req.id, json!({"ok": true})),
                "resources/subscribe" => {
                    let uri = req
                        .params
                        .as_ref()
                        .and_then(|p| p["uri"].as_str())
                        .unwrap_or("");
                    // The gate: only `res://ok` is subscribable here.
                    if uri == "res://ok" {
                        register_subscriber(&self.subs, uri, conn, writer);
                        Response::ok(req.id, json!({}))
                    } else {
                        Response::err(req.id, rpc::RESOURCE_NOT_FOUND, "no")
                    }
                }
                _ => Response::err(req.id, rpc::METHOD_NOT_FOUND, "unknown"),
            }
        }
    }

    fn http_post(addr: &str, body: &str) -> (Vec<(String, String)>, String) {
        let mut s = TcpStream::connect(addr).unwrap();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut reader = BufReader::new(s);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut l = String::new();
            reader.read_line(&mut l).unwrap();
            if l.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let mut body = String::new();
        reader.read_to_string(&mut body).unwrap();
        (headers, body)
    }

    fn spawn_server() -> (String, SubRegistry) {
        let subs: SubRegistry = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let listener = bind_tcp("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_accept_http(
            listener,
            Arc::new(HttpAcceptor::Plain),
            Arc::new(TestHandler {
                subs: Arc::clone(&subs),
            }),
            Arc::new(AllowAll),
            Arc::clone(&subs),
            Arc::new(AtomicU64::new(0)),
            Duration::from_secs(5),
        )
        .unwrap();
        (addr, subs)
    }

    #[test]
    fn unary_post_returns_application_json() {
        let (addr, _subs) = spawn_server();
        let (headers, body) = http_post(
            &addr,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#,
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "content-type" && v.contains("application/json")),
            "headers: {headers:?}"
        );
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    /// POST with an explicit `Origin` header; returns the HTTP status code.
    fn http_post_origin(addr: &str, origin: Option<&str>, body: &str) -> u16 {
        let mut s = TcpStream::connect(addr).unwrap();
        let origin_line = origin
            .map(|o| format!("Origin: {o}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "POST /mcp HTTP/1.1\r\nHost: x\r\n{origin_line}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut status = String::new();
        BufReader::new(s).read_line(&mut status).unwrap();
        status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    #[test]
    fn initialize_stamps_a_unique_session_header() {
        let (addr, _subs) = spawn_server();
        let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#;
        let sid = |addr: &str| {
            let (headers, _) = http_post(addr, init);
            headers
                .into_iter()
                .find(|(k, _)| k == "mcp-session-id")
                .map(|(_, v)| v)
        };
        let a = sid(&addr).expect("initialize stamps a session id");
        let b = sid(&addr).expect("second initialize stamps a session id");
        assert_ne!(a, "srv", "the session id is not the old constant");
        assert_ne!(a, b, "each initialize mints a distinct session id");
    }

    #[test]
    fn a_cross_origin_request_is_rejected_403() {
        let (addr, _subs) = spawn_server();
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#;
        // A browser cross-site Origin → 403 (DNS-rebinding defense).
        assert_eq!(
            http_post_origin(&addr, Some("https://evil.example"), call),
            403
        );
        // No Origin (the normal non-browser caller) → served (200).
        assert_eq!(http_post_origin(&addr, None, call), 200);
        // A loopback Origin (a local dev tool) → served.
        assert_eq!(
            http_post_origin(&addr, Some("http://localhost:3000"), call),
            200
        );
        assert_eq!(http_post_origin(&addr, Some("http://127.0.0.1"), call), 200);
    }

    /// A server with an extra allowed origin configured.
    fn spawn_server_with_origins(extra: &[&str]) -> String {
        let subs: SubRegistry = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let listener = bind_tcp("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_accept_http_opts(
            listener,
            Arc::new(HttpAcceptor::Plain),
            Arc::new(TestHandler {
                subs: Arc::clone(&subs),
            }),
            Arc::new(AllowAll),
            Arc::clone(&subs),
            Arc::new(AtomicU64::new(0)),
            Duration::from_secs(5),
            ServeOptions {
                extra_origins: extra.iter().map(|s| s.to_string()).collect(),
            },
        )
        .unwrap();
        addr
    }

    /// A raw request; returns (status code, lowercased headers).
    fn http_raw(addr: &str, req: &str) -> (u16, Vec<(String, String)>) {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(req.as_bytes()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut reader = BufReader::new(s);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        let code = status
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut headers = Vec::new();
        loop {
            let mut l = String::new();
            if reader.read_line(&mut l).unwrap_or(0) == 0 {
                break;
            }
            if l.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        (code, headers)
    }

    #[test]
    fn a_configured_extra_origin_is_served_with_cors_headers() {
        let addr = spawn_server_with_origins(&["https://ui.example"]);
        let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"x"}}"#;
        // The configured origin is allowed AND gets the CORS grant echoed.
        let req = format!(
            "POST / HTTP/1.1\r\nHost: x\r\nOrigin: https://ui.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call}",
            call.len()
        );
        let (code, headers) = http_raw(&addr, &req);
        assert_eq!(code, 200);
        assert!(
            headers
                .iter()
                .any(|(k, v)| { k == "access-control-allow-origin" && v == "https://ui.example" }),
            "CORS echo missing: {headers:?}"
        );
        // A different cross-site origin stays rejected.
        let bad = format!(
            "POST / HTTP/1.1\r\nHost: x\r\nOrigin: https://evil.example\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call}",
            call.len()
        );
        assert_eq!(http_raw(&addr, &bad).0, 403);
        // A loopback origin also gets the CORS echo (a local web UI on another port).
        let local = format!(
            "POST / HTTP/1.1\r\nHost: x\r\nOrigin: http://127.0.0.1:5173\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{call}",
            call.len()
        );
        let (code, headers) = http_raw(&addr, &local);
        assert_eq!(code, 200);
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "access-control-allow-origin" && v == "http://127.0.0.1:5173")
        );
    }

    #[test]
    fn a_cors_preflight_options_is_answered_before_auth() {
        let addr = spawn_server_with_origins(&["https://ui.example"]);
        let req = "OPTIONS / HTTP/1.1\r\nHost: x\r\nOrigin: https://ui.example\r\nAccess-Control-Request-Method: POST\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (code, headers) = http_raw(&addr, req);
        assert_eq!(code, 204);
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "access-control-allow-origin" && v == "https://ui.example")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| k == "access-control-allow-headers" && v.contains("authorization"))
        );
        // A preflight from a denied origin is 403 (the rebind gate holds).
        let bad = "OPTIONS / HTTP/1.1\r\nHost: x\r\nOrigin: https://evil.example\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        assert_eq!(http_raw(&addr, bad).0, 403);
    }

    #[test]
    fn origin_loopback_classification() {
        assert!(origin_is_loopback("http://localhost"));
        assert!(origin_is_loopback("http://localhost:8080"));
        assert!(origin_is_loopback("https://127.0.0.1:443"));
        assert!(origin_is_loopback("http://[::1]:9000"));
        assert!(!origin_is_loopback("https://evil.example"));
        assert!(!origin_is_loopback("http://169.254.1.1")); // link-local, not loopback
        assert!(!origin_is_loopback("null")); // opaque origin → untrusted
    }

    #[test]
    fn subscriptions_listen_streams_a_pushed_update_as_sse() {
        let (addr, subs) = spawn_server();
        // Open the SSE stream in a thread; it stays open, so read incrementally.
        let addr2 = addr.clone();
        let got = Arc::new(Mutex::new(String::new()));
        let got2 = Arc::clone(&got);
        thread::spawn(move || {
            let mut s = TcpStream::connect(&addr2).unwrap();
            let body = r#"{"jsonrpc":"2.0","id":1,"method":"subscriptions/listen","params":{"notifications":{"resourceSubscriptions":["res://ok"]}}}"#;
            let req = format!(
                "POST /mcp HTTP/1.1\r\nHost: x\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(req.as_bytes()).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut reader = BufReader::new(s);
            let mut line = String::new();
            // Read until we see a data: line or time out.
            for _ in 0..50 {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line.starts_with("data:") {
                    *got2.lock().unwrap() = line.clone();
                    break;
                }
            }
        });

        // Wait for the subscription to register, then push an update.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if subs.lock().unwrap().contains_key("res://ok") {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        notify_resource_updated_keep(&subs, "res://ok");

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if got.lock().unwrap().starts_with("data:") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no SSE push observed");
            thread::sleep(Duration::from_millis(20));
        }
        let data = got.lock().unwrap().clone();
        assert!(data.contains("notifications/resources/updated"), "{data}");
        assert!(data.contains("res://ok"), "{data}");
    }
}

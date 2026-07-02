// SPDX-License-Identifier: Apache-2.0
//! The **Streamable HTTP** MCP server: HTTP/1.1 + SSE over TCP (plain, or TLS via
//! the [`net::tls`] acceptor), reusing the same [`Handler`] / [`lifecycle_response`]
//! / [`SubRegistry`](crate::server::SubRegistry) as the socket servers. This is the
//! serving mirror of the crate's HTTP *client* ([`crate::http`]) and the transport
//! the HTTPS control plane rides.
//!
//! Model (RFC 0004 Streamable HTTP, both eras):
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

/// The parts of an inbound request an [`HttpAuth`] classifies trust from.
pub struct RequestParts<'a> {
    /// The request's headers (lowercased names), e.g. to read `authorization`.
    pub headers: &'a [(String, String)],
    /// Whether the peer presented a verified client certificate (mutual TLS).
    pub peer_cert: bool,
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
    thread::Builder::new()
        .name("serve-http".into())
        .spawn(move || {
            for tcp in listener.incoming().flatten() {
                let acceptor = Arc::clone(&acceptor);
                let handler = Arc::clone(&handler);
                let auth = Arc::clone(&auth);
                let subs = Arc::clone(&subs);
                let conn_counter = Arc::clone(&conn_counter);
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
                        );
                    })
                    .ok();
            }
        })
        .map(|_| ())
}

fn accept_and_serve(
    tcp: TcpStream,
    acceptor: &HttpAcceptor,
    handler: &Arc<dyn Handler>,
    auth: &Arc<dyn HttpAuth>,
    subs: &SubRegistry,
    conn_counter: &AtomicU64,
    write_timeout: Duration,
) {
    let _ = tcp.set_write_timeout(Some(write_timeout));
    let _ = tcp.set_read_timeout(Some(write_timeout));
    match acceptor {
        HttpAcceptor::Plain => {
            serve_conn(tcp, false, handler, auth, subs, conn_counter);
        }
        // A failed TLS/mTLS handshake never reaches the protocol layer.
        #[cfg(feature = "tls")]
        HttpAcceptor::Tls(tls) => {
            if let Ok(stream) = tls.accept(tcp) {
                let peer_cert = net::tls::peer_presented_cert(&stream);
                serve_conn(stream, peer_cert, handler, auth, subs, conn_counter);
            }
        }
    }
}

/// Serve one accepted (already TLS-terminated) connection. Generic over the
/// concrete stream so plain TCP and the TLS stream share one code path.
fn serve_conn<S: Read + Write + Send + 'static>(
    stream: S,
    peer_cert: bool,
    handler: &Arc<dyn Handler>,
    auth: &Arc<dyn HttpAuth>,
    subs: &SubRegistry,
    conn_counter: &AtomicU64,
) {
    let mut reader = BufReader::new(stream);
    let Some(req) = read_request(&mut reader) else {
        return; // malformed / EOF before a full request
    };

    // Trust classification — the transport is never the boundary.
    let origin = {
        let parts = RequestParts {
            headers: &req.headers,
            peer_cert,
        };
        auth.authenticate(&parts)
    };
    let Some(origin) = origin else {
        let _ = write_simple(reader.get_mut(), 401, "Unauthorized", b"");
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
        );
        return;
    }

    let conn = conn_counter.fetch_add(1, Ordering::Relaxed);
    handler.on_connect(origin, conn);

    let incoming: Result<Incoming, _> = serde_json::from_slice(&req.body);
    match incoming {
        Ok(Incoming::Request(rpc_req)) if rpc_req.method == method::SUBSCRIPTIONS_LISTEN => {
            serve_listen(reader, rpc_req, origin, conn, handler, subs);
        }
        // A server-streaming method (the embedder declares them — e.g. the A2A
        // streaming pair): the response is an SSE stream of JSON-RPC frames.
        Ok(Incoming::Request(rpc_req)) if handler.streams(&rpc_req.method) => {
            serve_stream(reader, rpc_req, origin, conn, handler);
            remove_and_disconnect(subs, conn, origin, handler);
        }
        Ok(Incoming::Request(rpc_req)) => {
            serve_unary(reader.get_mut(), rpc_req, origin, conn, handler);
            remove_and_disconnect(subs, conn, origin, handler);
        }
        // A notification POST (e.g. notifications/initialized) → 202, no body.
        Ok(Incoming::Notification(_)) | Ok(Incoming::Response(_)) => {
            let _ = write_simple(reader.get_mut(), 202, "Accepted", b"");
            remove_and_disconnect(subs, conn, origin, handler);
        }
        Err(_) => {
            let _ = write_simple(reader.get_mut(), 400, "Bad Request", b"invalid JSON-RPC frame");
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
) {
    let mut stream = reader.into_inner();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).and_then(|_| stream.flush()).is_err() {
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
                    .map(|mut w| w.write_all(b": keep-alive\n\n").and_then(|_| w.flush()).is_ok())
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
) {
    // A null sink for the dispatch's `writer` arg: unary methods don't push, and
    // a stray write must never corrupt the HTTP response.
    let sink: SharedWriter = Arc::new(Mutex::new(ServeStream::Http(Box::new(io::sink()))));
    let is_initialize = req.method == method::INITIALIZE;
    let resp = handler.dispatch(req, origin, &sink, conn);
    let body = serde_json::to_vec(&resp).unwrap_or_default();
    let session = if is_initialize {
        "Mcp-Session-Id: srv\r\n"
    } else {
        ""
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n",
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
) {
    let uris = listen_uris(&req);
    let mut stream = reader.into_inner();
    // SSE response head.
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).and_then(|_| stream.flush()).is_err() {
        remove_and_disconnect(subs, conn, origin, handler);
        return;
    }

    // The connection's write half becomes the shared SSE sink. Registration goes
    // through the handler's own subscribe gate (a synthetic resources/subscribe
    // per uri), so this reuses the embedder's subscribability rules verbatim.
    let writer: SharedWriter = Arc::new(Mutex::new(ServeStream::Http(Box::new(stream))));
    for uri in &uris {
        let sub_req = Request::new(
            0,
            method::RESOURCES_SUBSCRIBE,
            Some(json!({ "uri": uri })),
        );
        let _ = handler.dispatch(sub_req, origin, &writer, conn);
    }

    // Hold the stream open, using keep-alive comments as the disconnect probe.
    loop {
        thread::sleep(SSE_KEEPALIVE);
        let alive = writer
            .lock()
            .map(|mut w| w.write_all(b": keep-alive\n\n").and_then(|_| w.flush()).is_ok())
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
fn write_simple<S: Write>(stream: &mut S, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
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

/// Read one HTTP/1.1 request (request line, headers, `Content-Length` body).
/// Returns `None` on EOF-before-request or a malformed head.
fn read_request<S: Read>(reader: &mut BufReader<S>) -> Option<HttpRequest> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();

    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((k, v)) = line.split_once(':') {
            let name = k.trim().to_ascii_lowercase();
            let value = v.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    if content_length > MAX_BODY {
        return None;
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(HttpRequest {
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
                    let uri = req.params.as_ref().and_then(|p| p["uri"].as_str()).unwrap_or("");
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

    #[test]
    fn initialize_stamps_a_session_header() {
        let (addr, _subs) = spawn_server();
        let (headers, _body) = http_post(
            &addr,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#,
        );
        assert!(
            headers.iter().any(|(k, _)| k == "mcp-session-id"),
            "initialize must stamp a session id: {headers:?}"
        );
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

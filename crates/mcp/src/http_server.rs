// SPDX-License-Identifier: AGPL-3.0-only
//! A **raw HTTP/1.1 listener** over TCP (plain, or TLS via the [`net::tls`]
//! acceptor) for embedders that serve plain HTTP rather than MCP — agentd's
//! webhook listener. Each request is read under hard head/body bounds, passed
//! through a DNS-rebinding `Origin` guard, and handed whole to the embedder's
//! [`RawHandler`], which routes and authenticates it itself (e.g. a per-webhook
//! HMAC over the raw body). One request per connection (`Connection: close`).

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Cap on a request body (webhook payloads are small; this bounds a hostile peer).
const MAX_BODY: usize = 8 * 1024 * 1024;

/// Cap on the whole request HEAD — request line plus every header line. A body
/// is bounded by its declared `Content-Length`; a head is bounded by nothing the
/// peer tells us, so it has to be bounded by us: without this, one connection
/// that never sends a newline grows a `String` until the process dies, and it
/// can do that BEFORE authenticating (the head is read to find the credential).
const MAX_HEAD_BYTES: usize = 64 * 1024;

/// Cap on the number of header lines kept. The byte cap already bounds the
/// total, but 64 KiB of four-byte headers is still ~13k `Vec` entries and every
/// `RawRequest::header` lookup is a linear scan over them — so bound the count
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

/// How accepted TCP connections are wrapped: plaintext (loopback dev) or TLS
/// (production). The TLS variant carries the [`net::tls`] acceptor, which drives
/// the handshake (and, under mTLS, verifies the client certificate) at accept
/// time.
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

/// Lift the verified mTLS peer's identity (subject CN + SANs) so the embedder's
/// own authentication can match it to a principal. `default()` when no client
/// cert was presented — an absent identity must read as "unidentified", never
/// as a match.
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

/// A minimal status response with a short text body — the listener's own
/// refusals, written before any handler runs.
fn write_simple<S: Write>(stream: &mut S, code: u16, reason: &str, body: &[u8]) -> io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// A raw inbound HTTP request handed straight to a [`RawHandler`]: method, target
/// (path + optional query), lowercased headers, and the raw body. No JSON-RPC
/// parsing and no transport-level auth — the embedder routes by
/// [`RawRequest::path`] and authenticates itself (e.g. a per-webhook HMAC over
/// the raw body). The DNS-rebind `Origin` guard and TLS termination still apply.
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

/// Spawn a raw-HTTP accept loop — one blocking thread per connection,
/// TLS-terminated per `acceptor`, behind the DNS-rebind `Origin` guard —
/// dispatching each request to `handler`.
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
            // A refused head is answered, not just dropped: 431 is a real answer
            // a client can act on, and it costs nothing — the peer never got
            // past the reader, so no handler was involved.
            let _ = write_simple(
                reader.get_mut(),
                431,
                "Request Header Fields Too Large",
                b"request head exceeds the header size/count limits",
            );
            return;
        }
        Err(ReadError::Incomplete) => return, // malformed / EOF before a full request
    };
    // DNS-rebinding defense: a browser always sends `Origin`, so a page tricked
    // into POSTing to a local listener carries its own site there. Webhook
    // callers are servers, not browsers — so only a loopback origin passes, and
    // a caller that sends none is unaffected.
    if matches!(check_origin(&req.headers), OriginCheck::Denied) {
        let _ = write_simple(
            reader.get_mut(),
            403,
            "Forbidden",
            b"cross-origin request rejected",
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
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// The DNS-rebinding gate's verdict on a request's `Origin` header.
enum OriginCheck {
    /// No `Origin` header — a non-browser caller.
    NoBrowser,
    /// A loopback browser origin (a local tool).
    Allowed,
    /// A cross-site browser origin — reject 403.
    Denied,
}

/// Classify a request's `Origin` (if any) — the DNS-rebinding gate. No `Origin`
/// header → a non-browser caller, allowed. Present → it must name a loopback
/// origin.
fn check_origin(headers: &[(String, String)]) -> OriginCheck {
    match headers.iter().find(|(k, _)| k == "origin") {
        None => OriginCheck::NoBrowser,
        Some((_, origin)) if origin_is_loopback(origin) => OriginCheck::Allowed,
        Some(_) => OriginCheck::Denied,
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
    use std::net::TcpStream;

    /// Answers every request 200 with its path, so a test can tell a served
    /// request from a refused one.
    struct Echo;
    impl RawHandler for Echo {
        fn handle(&self, req: &RawRequest) -> RawResponse {
            RawResponse::text(200, "OK", req.path().to_string())
        }
    }

    fn spawn_server() -> String {
        let listener = bind_tcp("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        spawn_accept_raw(
            listener,
            Arc::new(HttpAcceptor::Plain),
            Arc::new(Echo),
            Duration::from_secs(5),
        )
        .unwrap();
        addr
    }

    /// POST with an optional `Origin` header; returns the HTTP status code.
    fn http_post_origin(addr: &str, origin: Option<&str>) -> u16 {
        let mut s = TcpStream::connect(addr).unwrap();
        let origin_line = origin
            .map(|o| format!("Origin: {o}\r\n"))
            .unwrap_or_default();
        let body = "{}";
        let req = format!(
            "POST /hook HTTP/1.1\r\nHost: x\r\n{origin_line}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
    fn a_cross_origin_request_is_rejected_403() {
        let addr = spawn_server();
        // A browser cross-site Origin → 403 (DNS-rebinding defense).
        assert_eq!(http_post_origin(&addr, Some("https://evil.example")), 403);
        // No Origin (the normal non-browser caller) → served (200).
        assert_eq!(http_post_origin(&addr, None), 200);
        // A loopback Origin (a local dev tool) → served.
        assert_eq!(http_post_origin(&addr, Some("http://localhost:3000")), 200);
        assert_eq!(http_post_origin(&addr, Some("http://127.0.0.1")), 200);
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
}

// SPDX-License-Identifier: Apache-2.0
//! A minimal blocking HTTP/1.1 client over any `Read + Write`. RFC 0006.
//!
//! This is the single highest-leverage minimalism decision (RFC 0002): one
//! ~250-line module replaces the `ureq`/`url`→IDNA→ICU dependency tax. It
//! carries the intelligence wire over TCP, TLS, unix sockets, and vsock
//! alike — the transport is just the stream.
//!
//! Two request paths: [`send`] buffers the whole response (the LLM/intelligence
//! path), and [`send_streaming`] returns the status + headers plus a live reader
//! so the caller can either buffer it (`application/json`) or pump it as an **SSE**
//! stream ([`SseReader`]) — the MCP Streamable HTTP transport, where a response may
//! be a single JSON body or a `text/event-stream`, and a long-lived GET carries
//! server→client notifications.
//! `connect_tcp` is intentionally unguarded: agentd's HTTP client only ever
//! dials the *operator-configured* intelligence endpoint, which the model
//! cannot influence. The SSRF classifier (`net::ssrf`) exists for any future
//! model/agent-supplied URL surface and is composed at the call site that
//! needs it.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Response body cap. LLM responses can be large; 8 MiB is generous without
/// being an unbounded allocation from a hostile peer.
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// Any bidirectional byte stream the HTTP client can run over. `Box<dyn Stream>`
/// is itself `Read + Write` (via std's `impl<R: Read + ?Sized> Read for Box<R>`),
/// so an OWNED boxed stream can be handed to [`send_streaming`] by value — used
/// by the long-lived MCP notification SSE reader.
pub trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

/// A parsed absolute URL (the subset we need: scheme/host/port/path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path + query, always starting with `/`.
    pub path: String,
}

impl Url {
    /// Parse `http(s)://host[:port][/path][?query]`. No `url` crate — we only
    /// support the absolute http/https forms agentd actually issues.
    pub fn parse(s: &str) -> Result<Url, String> {
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| format!("not an absolute URL: {s}"))?;
        let scheme = scheme.to_ascii_lowercase();
        let default_port = match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            other => return Err(format!("unsupported scheme: {other}")),
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(format!("missing host in URL: {s}"));
        }
        let (host, port) = match authority.rsplit_once(':') {
            // ':' only counts as a port separator if what follows is numeric
            // (guards against IPv6 literals, which we don't expect here).
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (
                h.to_string(),
                p.parse().map_err(|_| format!("bad port in {s}"))?,
            ),
            _ => (authority.to_string(), default_port),
        };
        Ok(Url {
            scheme,
            host,
            port,
            path: path.to_string(),
        })
    }

    pub fn is_tls(&self) -> bool {
        self.scheme == "https"
    }

    /// The `Host:` header value (includes a non-default port).
    pub fn host_header(&self) -> String {
        let default = if self.is_tls() { 443 } else { 80 };
        if self.port == default {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// A parsed HTTP response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// Case-insensitive header lookup (header names are stored lowercased).
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn body_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }
}

/// Whether `host` names the local loopback — the dev/test carve-out for
/// plaintext `http://` (production transports are TLS-only). Accepts the IPv4
/// loopback block (`127.0.0.0/8`), the IPv6 loopback (`::1`, bare or
/// bracketed), and the literal name `localhost`. A resolvable-but-unresolved
/// name is NOT loopback — this classifies the written form, without DNS.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    if h.eq_ignore_ascii_case("localhost") {
        return true;
    }
    h.parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

/// Connect a plain TCP stream with connect + read/write timeouts. Intentionally
/// unguarded — the only caller dials the operator-configured endpoint; the SSRF
/// classifier (`net::ssrf`) is composed at any model/agent-supplied URL surface.
pub fn connect_tcp(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let addr = (host, port).to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("cannot resolve {host}:{port}"),
        )
    })?;
    let stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true).ok();
    Ok(stream)
}

/// Issue one request over `stream` and read the full response. Adds `Host`,
/// `Connection: close`, and `Content-Length`; the caller supplies any other
/// headers (e.g. `Authorization`, `Content-Type`).
pub fn send<S: Read + Write + ?Sized>(
    stream: &mut S,
    host_header: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<Response> {
    let mut req: Vec<u8> = Vec::with_capacity(256 + body.len());
    write!(req, "{method} {path} HTTP/1.1\r\n")?;
    write!(req, "Host: {host_header}\r\n")?;
    req.extend_from_slice(b"Connection: close\r\n");
    for (k, v) in headers {
        // Reject CR/LF injection in caller-supplied headers (RFC 0012).
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CR/LF in header",
            ));
        }
        write!(req, "{k}: {v}\r\n")?;
    }
    write!(req, "Content-Length: {}\r\n\r\n", body.len())?;
    req.extend_from_slice(body);
    stream.write_all(&req)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    read_response(&mut reader)
}

/// Read the status line + headers off a response, leaving the reader positioned
/// at the start of the body. Header names are lowercased.
fn read_head<R: BufRead>(r: &mut R) -> io::Result<(u16, Vec<(String, String)>)> {
    let mut status_line = String::new();
    r.read_line(&mut status_line)?;
    let status = parse_status(&status_line)?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    Ok((status, headers))
}

fn read_response<R: BufRead>(r: &mut R) -> io::Result<Response> {
    let (status, headers) = read_head(r)?;

    let content_length = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok());
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));

    let body = if chunked {
        read_chunked(r)?
    } else if let Some(n) = content_length {
        read_exact_capped(r, n)?
    } else {
        // Connection: close — read to EOF, capped.
        read_to_end_capped(r)?
    };

    Ok(Response {
        status,
        headers,
        body,
    })
}

fn parse_status(line: &str) -> io::Result<u16> {
    // "HTTP/1.1 200 OK"
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bad status line: {line:?}"),
            )
        })
}

fn read_exact_capped<R: Read>(r: &mut R, n: usize) -> io::Result<Vec<u8>> {
    if n > MAX_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds cap",
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_to_end_capped<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(MAX_RESPONSE as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > MAX_RESPONSE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds cap",
        ));
    }
    Ok(buf)
}

fn read_chunked<R: BufRead>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        r.read_line(&mut size_line)?;
        let size_hex = size_line.trim_end_matches(['\r', '\n']);
        // A chunk extension (`;name=val`) may follow the size.
        let size_hex = size_hex.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
        if size == 0 {
            // Consume the trailing CRLF (and any trailers) until blank line.
            loop {
                let mut t = String::new();
                if r.read_line(&mut t)? == 0 || t.trim_end_matches(['\r', '\n']).is_empty() {
                    break;
                }
            }
            break;
        }
        if body.len() + size > MAX_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked response exceeds cap",
            ));
        }
        let mut chunk = vec![0u8; size];
        r.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        // Trailing CRLF after the chunk.
        let mut crlf = [0u8; 2];
        r.read_exact(&mut crlf)?;
    }
    Ok(body)
}

/// A streamed response: status + headers, plus the reader positioned at the body.
/// The caller decides how to drain it — [`into_body`] to buffer (`application/json`)
/// or [`sse`] to pump it as an SSE event stream (`text/event-stream`). Owns the
/// underlying stream, matching the MCP client's per-request connection model.
pub struct StreamingResponse<S: Read + Write> {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    reader: BufReader<S>,
}

impl<S: Read + Write> StreamingResponse<S> {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
    /// The lowercased `Content-Type` (media type only, params stripped).
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
            .map(|v| v.split(';').next().unwrap_or(v).trim())
    }
    /// `true` when the body is `text/event-stream` (Streamable HTTP SSE).
    pub fn is_event_stream(&self) -> bool {
        self.content_type() == Some("text/event-stream")
    }
    /// Buffer the whole body (capped), honoring `Content-Length`/`chunked`/close.
    pub fn into_body(mut self) -> io::Result<Vec<u8>> {
        let content_length = self
            .headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .and_then(|(_, v)| v.parse::<usize>().ok());
        let chunked = self
            .headers
            .iter()
            .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"));
        if chunked {
            read_chunked(&mut self.reader)
        } else if let Some(n) = content_length {
            read_exact_capped(&mut self.reader, n)
        } else {
            read_to_end_capped(&mut self.reader)
        }
    }
    /// Consume into an [`SseReader`] to pump `text/event-stream` events.
    pub fn sse(self) -> SseReader<BufReader<S>> {
        SseReader::new(self.reader)
    }
}

/// Issue one request over an OWNED `stream` and return the status + headers +
/// body reader WITHOUT draining the body (unlike [`send`]). Adds `Host`,
/// `Connection: close`, and `Content-Length`; the caller supplies the rest
/// (`Accept`, `Authorization`, `Content-Type`, `Mcp-Session-Id`, …).
pub fn send_streaming<S: Read + Write>(
    mut stream: S,
    host_header: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<StreamingResponse<S>> {
    let mut req: Vec<u8> = Vec::with_capacity(256 + body.len());
    write!(req, "{method} {path} HTTP/1.1\r\n")?;
    write!(req, "Host: {host_header}\r\n")?;
    req.extend_from_slice(b"Connection: close\r\n");
    for (k, v) in headers {
        if k.contains(['\r', '\n']) || v.contains(['\r', '\n']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CR/LF in header",
            ));
        }
        write!(req, "{k}: {v}\r\n")?;
    }
    write!(req, "Content-Length: {}\r\n\r\n", body.len())?;
    req.extend_from_slice(body);
    stream.write_all(&req)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let (status, headers) = read_head(&mut reader)?;
    Ok(StreamingResponse {
        status,
        headers,
        reader,
    })
}

/// One parsed SSE event (RFC 6455-style `text/event-stream`). For MCP, `data` is
/// a JSON-RPC message; `event`/`id` are the optional SSE field lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// A blocking, line-based SSE reader. `next_event` accumulates `field: value`
/// lines and emits one [`SseEvent`] per blank-line separator (multiple `data:`
/// lines join with `\n`), returning `Ok(None)` at end of stream. Bounded per
/// event by [`MAX_RESPONSE`] so a hostile stream cannot exhaust memory.
pub struct SseReader<R: BufRead> {
    r: R,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(r: R) -> SseReader<R> {
        SseReader { r }
    }

    /// Read the next event, or `Ok(None)` at EOF. Comment lines (`:` prefix) and
    /// unknown fields are ignored per the SSE spec.
    pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
        let mut ev = SseEvent::default();
        let mut saw_field = false;
        let mut total = 0usize;
        loop {
            let mut line = String::new();
            let n = self.r.read_line(&mut line)?;
            if n == 0 {
                // EOF: flush a pending event if one was in progress.
                return Ok(if saw_field { Some(ev) } else { None });
            }
            total += n;
            if total > MAX_RESPONSE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "SSE event exceeds cap",
                ));
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                // Blank line dispatches the accumulated event.
                if saw_field {
                    return Ok(Some(ev));
                }
                continue; // stray blank line between events
            }
            if line.starts_with(':') {
                continue; // comment
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""), // a bare field name with empty value
            };
            saw_field = true;
            match field {
                "event" => ev.event = Some(value.to_string()),
                "id" => ev.id = Some(value.to_string()),
                "data" => {
                    if !ev.data.is_empty() {
                        ev.data.push('\n');
                    }
                    ev.data.push_str(value);
                }
                _ => {} // retry/unknown — ignore
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn url_parse_https_default_port() {
        let u = Url::parse("https://api.openai.com/v1/chat/completions").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "api.openai.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path, "/v1/chat/completions");
        assert_eq!(u.host_header(), "api.openai.com");
        assert!(u.is_tls());
    }

    #[test]
    fn url_parse_http_with_port_and_no_path() {
        let u = Url::parse("http://localhost:8080").unwrap();
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/");
        assert_eq!(u.host_header(), "localhost:8080");
        assert!(!u.is_tls());
    }

    #[test]
    fn url_rejects_bad_scheme() {
        assert!(Url::parse("ftp://x/").is_err());
        assert!(Url::parse("no-scheme").is_err());
    }

    #[test]
    fn response_content_length() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"ok\":true}!!";
        let mut cur = Cursor::new(raw.as_bytes().to_vec());
        let resp = read_response(&mut cur).unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("content-type"), Some("application/json"));
        assert_eq!(resp.body, b"{\"ok\":true}!!");
        assert!(resp.is_success());
    }

    #[test]
    fn response_chunked() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut cur = Cursor::new(raw.as_bytes().to_vec());
        let resp = read_response(&mut cur).unwrap();
        assert_eq!(resp.body, b"hello world");
    }

    #[test]
    fn cr_lf_header_injection_rejected() {
        let mut sink: Vec<u8> = Vec::new();
        // a write-only fake stream: Cursor over Vec implements Write+Read
        let mut stream = Cursor::new(Vec::new());
        let _ = &mut sink;
        let err = send(&mut stream, "h", "POST", "/", &[("X", "a\r\nEvil: 1")], b"").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// A fake duplex stream: reads return a canned server response, writes are
    /// captured (so a request/response round-trip is testable without sockets).
    struct FakeStream {
        resp: Cursor<Vec<u8>>,
        sink: Vec<u8>,
    }
    impl Read for FakeStream {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.resp.read(buf)
        }
    }
    impl Write for FakeStream {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.sink.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn sse_events(body: &str) -> Vec<SseEvent> {
        let mut r = SseReader::new(BufReader::new(Cursor::new(body.as_bytes().to_vec())));
        let mut out = Vec::new();
        while let Some(e) = r.next_event().unwrap() {
            out.push(e);
        }
        out
    }

    #[test]
    fn sse_parses_events_with_event_id_and_data() {
        let body = "event: message\nid: 7\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let evs = sse_events(body);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].event.as_deref(), Some("message"));
        assert_eq!(evs[0].id.as_deref(), Some("7"));
        assert_eq!(evs[0].data, "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}");
    }

    #[test]
    fn sse_joins_multi_data_lines_and_ignores_comments() {
        // Comment line, then an event whose data spans two `data:` lines.
        let body = ": keep-alive\ndata: line1\ndata: line2\n\ndata: second\n\n";
        let evs = sse_events(body);
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].data, "line1\nline2");
        assert_eq!(evs[1].data, "second");
    }

    #[test]
    fn sse_flushes_trailing_event_without_final_blank_line() {
        let evs = sse_events("data: only\n");
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].data, "only");
    }

    #[test]
    fn send_streaming_reads_head_then_buffers_json_body() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: abc123\r\nContent-Length: 11\r\n\r\n{\"ok\":true}".to_string();
        let stream = FakeStream {
            resp: Cursor::new(raw.into_bytes()),
            sink: Vec::new(),
        };
        let resp = send_streaming(
            stream,
            "h",
            "POST",
            "/mcp",
            &[("Accept", "application/json")],
            b"{}",
        )
        .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.is_success());
        assert_eq!(resp.content_type(), Some("application/json"));
        assert!(!resp.is_event_stream());
        assert_eq!(resp.header("mcp-session-id"), Some("abc123"));
        assert_eq!(resp.into_body().unwrap(), b"{\"ok\":true}");
    }

    #[test]
    fn send_streaming_detects_event_stream_and_pumps_sse() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"x\":1}}\n\n".to_string();
        let stream = FakeStream {
            resp: Cursor::new(raw.into_bytes()),
            sink: Vec::new(),
        };
        let resp = send_streaming(stream, "h", "POST", "/mcp", &[], b"{}").unwrap();
        assert!(resp.is_event_stream());
        let mut sse = resp.sse();
        let ev = sse.next_event().unwrap().expect("one event");
        assert!(ev.data.contains("\"id\":1"));
        assert!(sse.next_event().unwrap().is_none());
    }
}

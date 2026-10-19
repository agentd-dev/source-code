// SPDX-License-Identifier: AGPL-3.0-only
//! A minimal blocking HTTP/1.1 client over any `Read + Write`.
//!
//! This is the single highest-leverage minimalism decision: one
//! ~250-line module replaces the `ureq`/`url`→IDNA→ICU dependency tax. It
//! carries the intelligence wire over TCP, TLS and unix sockets alike — the
//! transport is just the stream.
//!
//! Two request paths: [`send`] buffers the whole response (the LLM/intelligence
//! path), and [`send_streaming`] returns the status + headers plus a live reader
//! so the caller can either buffer it (`application/json`) or pump it as an **SSE**
//! stream ([`SseReader`]) — the MCP Streamable HTTP transport, where a response may
//! be a single JSON body or a `text/event-stream`, and a long-lived GET carries
//! server→client notifications.
//! `connect_tcp` is intentionally unguarded, and dials by *name*: it exists for
//! the *operator-configured* endpoints (the intelligence dial, the auth/token
//! endpoints), which the model cannot influence. A model/agent/peer-supplied URL
//! must NOT be dialled through it — not even with a guard wrapped around it,
//! since the guard's lookup and this function's lookup are two resolutions and a
//! hostile nameserver can answer them differently. Those surfaces use
//! [`crate::ssrf::connect_vetted`], which resolves once and dials the address it
//! vetted.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

    /// How long the server asked the client to wait (`Retry-After`), from
    /// now. See [`parse_retry_after`].
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")
            .and_then(|v| parse_retry_after(v, SystemTime::now()))
    }
}

/// The longest agentd takes a server's `Retry-After` at its word: a longer
/// wait is cut to this, so a server whose clock or arithmetic is off cannot
/// take itself out of a long-lived daemon until it restarts.
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(300);

/// Read a `Retry-After` value (RFC 9110 §10.2.3) as a wait from `now`: either
/// delay-seconds, or an HTTP-date in any of the three formats a recipient
/// must accept (§5.6.7) — IMF-fixdate, the obsolete RFC 850 form and asctime.
/// A date already past is no wait at all. `None` for anything else, which a
/// caller treats as a response without the header. The wait is the server's
/// word, unbounded here: each caller caps it to what it is willing to wait.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let v = value.trim();
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        // Digits only, so the one failure left is overflow: a wait longer
        // than u64 seconds is as good as for ever.
        return Some(Duration::from_secs(v.parse().unwrap_or(u64::MAX)));
    }
    let now = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    let at = http_date(v, now)?;
    Some(Duration::from_secs(at.saturating_sub(now).max(0) as u64))
}

/// An HTTP-date (RFC 9110 §5.6.7, any of its three formats) as seconds since
/// the Unix epoch; `now` (the same) places an RFC 850 two-digit year. A
/// response's `Date` is read with it as well as a `Retry-After`.
pub fn http_date(v: &str, now: i64) -> Option<i64> {
    let month = |m: &str| {
        [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .iter()
        .position(|n| *n == m)
        .map(|i| i as i64 + 1)
    };
    let clock = |t: &str| -> Option<i64> {
        let mut it = t.split(':');
        let (h, m, s) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() || [h, m, s].iter().any(|p| p.len() != 2) {
            return None;
        }
        let (h, m, s): (i64, i64, i64) = (h.parse().ok()?, m.parse().ok()?, s.parse().ok()?);
        (h < 24 && m < 60 && s < 61).then_some(h * 3600 + m * 60 + s)
    };
    let parts: Vec<&str> = v.split_ascii_whitespace().collect();
    let (year, mon, day, secs) = match parts.as_slice() {
        // IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`.
        [wd, d, m, y, t, "GMT"] if wd.ends_with(',') && wd.len() == 4 && y.len() == 4 => {
            (y.parse().ok()?, month(m)?, d.parse().ok()?, clock(t)?)
        }
        // RFC 850: `Sunday, 06-Nov-94 08:49:37 GMT`. A two-digit year more
        // than 50 years ahead is the century before (RFC 9110 §5.6.7).
        [wd, dmy, t, "GMT"] if wd.ends_with(',') => {
            let mut it = dmy.split('-');
            let (d, m, y) = (it.next()?, it.next()?, it.next()?);
            if it.next().is_some() || y.len() != 2 {
                return None;
            }
            let yy: i64 = y.parse().ok()?;
            let this = year_of(now.div_euclid(86_400));
            let mut year = this - this % 100 + yy;
            if year > this + 50 {
                year -= 100;
            }
            (year, month(m)?, d.parse().ok()?, clock(t)?)
        }
        // asctime: `Sun Nov  6 08:49:37 1994`.
        [_, m, d, t, y] if y.len() == 4 => (y.parse().ok()?, month(m)?, d.parse().ok()?, clock(t)?),
        _ => return None,
    };
    if !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, mon, day) * 86_400 + secs)
}

/// The proleptic Gregorian year of a day since 1970-01-01 (the year half of
/// H. Hinnant's `civil_from_days`).
fn year_of(days: i64) -> i64 {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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
/// unguarded, and resolves the name itself — for operator-configured endpoints
/// only. A model/agent/peer-supplied URL belongs on
/// [`crate::ssrf::connect_vetted`]: composing `ssrf::guard_host` around *this*
/// function resolves twice and is the DNS-rebinding hole, not a fix for it.
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
    // The request TARGET is caller-supplied too — a templated endpoint path or
    // a peer-supplied A2A push-notification URL — and CR/LF there splits the
    // request line exactly as it splits a header, letting an injected
    // `Authorization:` shadow the operator's real one further down. Scanned
    // before anything is written, so the framing layer stays closed on both
    // caller surfaces — the target as well as the headers.
    let mut req: Vec<u8> = Vec::with_capacity(256 + body.len());
    if path.contains(['\r', '\n']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CR/LF in request target",
        ));
    }
    write!(req, "{method} {path} HTTP/1.1\r\n")?;
    write!(req, "Host: {host_header}\r\n")?;
    req.extend_from_slice(b"Connection: close\r\n");
    for (k, v) in headers {
        // Reject CR/LF injection in caller-supplied headers: either would end
        // the current header line and let the rest be read as new headers.
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
        read_chunked(r, MAX_RESPONSE)?
    } else if let Some(n) = content_length {
        read_exact_capped(r, n, MAX_RESPONSE)?
    } else {
        // Connection: close — read to EOF, capped.
        read_to_end_capped(r, MAX_RESPONSE)?
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

fn read_exact_capped<R: Read>(r: &mut R, n: usize, cap: usize) -> io::Result<Vec<u8>> {
    if n > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds cap",
        ));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_to_end_capped<R: Read>(r: &mut R, cap: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(cap as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > cap {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response exceeds cap",
        ));
    }
    Ok(buf)
}

fn read_chunked<R: BufRead>(r: &mut R, cap: usize) -> io::Result<Vec<u8>> {
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
        // The declared size is peer-controlled and unverified until the bytes
        // actually arrive, so it must never be trusted with arithmetic OR with
        // an allocation. `size > cap` is checked first and the running
        // total uses `saturating_add`: the plain `body.len() + size` this
        // replaces wrapped on a size near `usize::MAX` — panicking the process
        // in debug, and in release wrapping *below* the cap so the check passed
        // and the allocation below aborted on a multi-exabyte request. That is
        // remotely reachable: `runtime::mod` dials every configured MCP server
        // at startup, so a hostile server could kill the daemon at connect time,
        // defeating the "a down server is only logged" containment.
        if size > cap || body.len().saturating_add(size) > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "chunked response exceeds cap",
            ));
        }
        // Read the chunk incrementally onto `body` instead of pre-allocating
        // `size` bytes, so even under the cap a lying header buys the peer an
        // allocation only as large as the bytes it really sends. A short read
        // means the peer framed a chunk it never delivered — that is a
        // truncated response, not an empty one, so it must not parse.
        let before = body.len();
        r.by_ref().take(size as u64).read_to_end(&mut body)?;
        if body.len() - before != size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated chunk",
            ));
        }
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
    /// How long the server asked the client to wait; see [`parse_retry_after`].
    pub fn retry_after(&self) -> Option<Duration> {
        self.header("retry-after")
            .and_then(|v| parse_retry_after(v, SystemTime::now()))
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
    /// Buffer the whole body (capped at [`MAX_RESPONSE`]), honoring
    /// `Content-Length`/`chunked`/close.
    pub fn into_body(self) -> io::Result<Vec<u8>> {
        self.into_body_within(MAX_RESPONSE)
    }
    /// [`Self::into_body`], capped at `cap` bytes: for a response the caller
    /// knows may be larger.
    pub fn into_body_within(mut self, cap: usize) -> io::Result<Vec<u8>> {
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
            read_chunked(&mut self.reader, cap)
        } else if let Some(n) = content_length {
            read_exact_capped(&mut self.reader, n, cap)
        } else {
            read_to_end_capped(&mut self.reader, cap)
        }
    }
    /// Consume into an [`SseReader`] to pump `text/event-stream` events.
    pub fn sse(self) -> SseReader<BufReader<S>> {
        SseReader::new(self.reader)
    }
    /// [`Self::sse`], each event capped at `cap` bytes.
    pub fn sse_within(self, cap: usize) -> SseReader<BufReader<S>> {
        SseReader::within(self.reader, cap)
    }

    /// Consume into the raw body reader — for a response that turned out NOT to
    /// be an event stream (e.g. a peer that answered `application/json`).
    pub fn into_reader(self) -> BufReader<S> {
        self.reader
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
    // Same request-target scan as [`send`] — this is the MCP path, where the
    // target comes from server-advertised endpoint metadata.
    let mut req: Vec<u8> = Vec::with_capacity(256 + body.len());
    if path.contains(['\r', '\n']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CR/LF in request target",
        ));
    }
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

/// One parsed `text/event-stream` event. For MCP, `data` is a JSON-RPC message;
/// `event`/`id` are the optional SSE field lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// A blocking, line-based SSE reader. `next_event` accumulates `field: value`
/// lines and emits one [`SseEvent`] per blank-line separator (multiple `data:`
/// lines join with `\n`), returning `Ok(None)` at end of stream. Bounded per
/// event by [`MAX_RESPONSE`] (or the cap it was made [`within`](Self::within))
/// so a hostile stream cannot exhaust memory.
pub struct SseReader<R: BufRead> {
    r: R,
    cap: usize,
}

impl<R: BufRead> SseReader<R> {
    pub fn new(r: R) -> SseReader<R> {
        SseReader::within(r, MAX_RESPONSE)
    }
    /// A reader whose every event is capped at `cap` bytes.
    pub fn within(r: R, cap: usize) -> SseReader<R> {
        SseReader { r, cap }
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
            if total > self.cap {
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

    /// `Retry-After` as delay-seconds and as each HTTP-date format, against
    /// a fixed clock: RFC 9110's own example date, 1994-11-06 08:49:37 GMT.
    #[test]
    fn retry_after_reads_seconds_and_every_http_date_form() {
        let at = UNIX_EPOCH + Duration::from_secs(784_111_777);
        let now = at - Duration::from_secs(120);
        let wait = |v| parse_retry_after(v, now);
        assert_eq!(wait("120"), Some(Duration::from_secs(120)));
        assert_eq!(wait(" 0 "), Some(Duration::ZERO));
        assert_eq!(
            wait("99999999999999999999999"),
            Some(Duration::from_secs(u64::MAX))
        );
        assert_eq!(
            wait("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            wait("Sun Nov  6 08:49:37 1994"),
            Some(Duration::from_secs(120))
        );
        // A date already past is no wait.
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT", at + Duration::from_secs(9)),
            Some(Duration::ZERO)
        );
        // Neither form: no header at all, as far as a caller is concerned.
        for bad in [
            "",
            "-5",
            "1.5",
            "soon",
            "Sun, 06 Nov 1994 08:49:37 PST",
            "Sun, 32 Nov 1994 08:49:37 GMT",
            "Sun, 06 Nov 1994 8:49:37 GMT",
        ] {
            assert_eq!(wait(bad), None, "{bad:?}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    }

    /// The obsolete RFC 850 form's two-digit year is read in the window RFC
    /// 9110 sets: no more than 50 years ahead of today.
    #[test]
    fn retry_after_reads_the_rfc_850_form() {
        // 2026-10-17 00:00:00 GMT.
        let now = UNIX_EPOCH + Duration::from_secs(1_792_195_200);
        assert_eq!(year_of(1_792_195_200 / 86_400), 2026);
        assert_eq!(year_of(-1), 1969);
        assert_eq!(
            parse_retry_after("Saturday, 17-Oct-26 00:02:00 GMT", now),
            Some(Duration::from_secs(120))
        );
        // `76` is 2076, fifty years ahead; `77` would be more, so it is 1977.
        assert!(
            parse_retry_after("Saturday, 17-Oct-76 00:00:00 GMT", now).unwrap() > Duration::ZERO
        );
        assert_eq!(
            parse_retry_after("Monday, 17-Oct-77 00:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        let r = Response {
            status: 429,
            headers: vec![("retry-after".into(), "7".into())],
            body: vec![],
        };
        assert_eq!(r.retry_after(), Some(Duration::from_secs(7)));
    }

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
    /// A body or an event past the cap fails; a caller that knows a response
    /// may be larger reads it within its own cap, in every framing.
    #[test]
    fn a_body_and_an_event_are_read_within_the_callers_cap() {
        let respond = |head: &str, body: &[u8]| {
            let mut raw = format!("HTTP/1.1 200 OK\r\n{head}\r\n").into_bytes();
            raw.extend_from_slice(body);
            let stream = FakeStream {
                resp: Cursor::new(raw),
                sink: Vec::new(),
            };
            send_streaming(stream, "h", "POST", "/mcp", &[], b"{}").unwrap()
        };
        let body = vec![b'x'; 64];
        let chunked = format!("40\r\n{}\r\n0\r\n\r\n", "x".repeat(64));
        for (head, bytes) in [
            ("Content-Length: 64\r\n", body.clone()),
            ("", body.clone()),
            ("Transfer-Encoding: chunked\r\n", chunked.into_bytes()),
        ] {
            let err = respond(head, &bytes).into_body_within(63).unwrap_err();
            assert!(err.to_string().contains("exceeds cap"), "{head:?}: {err}");
            assert_eq!(respond(head, &bytes).into_body_within(64).unwrap(), body);
        }
        let event = format!("data: {}\n\n", "y".repeat(64));
        let head = "Content-Type: text/event-stream\r\n";
        let mut small = respond(head, event.as_bytes()).sse_within(32);
        assert!(small.next_event().is_err());
        let mut large = respond(head, event.as_bytes()).sse_within(128);
        assert_eq!(large.next_event().unwrap().unwrap().data.len(), 64);
    }
}

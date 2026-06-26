//! A minimal blocking HTTP/1.1 client over any `Read + Write`. RFC 0006.
//!
//! This is the single highest-leverage minimalism decision (RFC 0002): one
//! ~250-line module replaces the `ureq`/`url`→IDNA→ICU dependency tax. It
//! carries the intelligence wire over TCP, TLS, unix sockets, and vsock
//! alike — the transport is just the stream.
//!
//! This client does non-streaming request/response only (`Connection: close`,
//! content-length or chunked); it does not implement SSE streaming.
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

/// Any bidirectional byte stream the HTTP client can run over.
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
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (h.to_string(), p.parse().map_err(|_| format!("bad port in {s}"))?)
            }
            _ => (authority.to_string(), default_port),
        };
        Ok(Url { scheme, host, port, path: path.to_string() })
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
        self.headers.iter().find(|(k, _)| *k == name).map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn body_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }
}

/// Connect a plain TCP stream with connect + read/write timeouts. Intentionally
/// unguarded — the only caller dials the operator-configured endpoint; the SSRF
/// classifier (`net::ssrf`) is composed at any model/agent-supplied URL surface.
pub fn connect_tcp(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    use std::net::ToSocketAddrs;
    let addr = (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {host}:{port}")))?;
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
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "CR/LF in header"));
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

fn read_response<R: BufRead>(r: &mut R) -> io::Result<Response> {
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

    Ok(Response { status, headers, body })
}

fn parse_status(line: &str) -> io::Result<u16> {
    // "HTTP/1.1 200 OK"
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("bad status line: {line:?}")))
}

fn read_exact_capped<R: Read>(r: &mut R, n: usize) -> io::Result<Vec<u8>> {
    if n > MAX_RESPONSE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "response exceeds cap"));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn read_to_end_capped<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(MAX_RESPONSE as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > MAX_RESPONSE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "response exceeds cap"));
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
            return Err(io::Error::new(io::ErrorKind::InvalidData, "chunked response exceeds cap"));
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
}

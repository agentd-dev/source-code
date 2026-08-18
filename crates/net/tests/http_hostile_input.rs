// SPDX-License-Identifier: Apache-2.0
//! Hostile-input tests for the blocking HTTP/1.1 client. Every byte parsed here
//! comes from a peer agentd dials but does not control — an intelligence
//! endpoint, an A2A peer, and (the reachable-at-startup one) every configured
//! MCP server, which `runtime::mod` connects to before the reactor loop opens.
//! A malformed response from any of them must become an `Err` the caller logs,
//! never a panic or an abort that takes the daemon down with it.

use net::http::{Response, send};
use std::io::{self, Cursor, Read, Write};

/// A fake duplex stream: reads return a canned server response, writes are
/// captured so we can assert what did (or did not) reach the wire.
struct FakeStream {
    resp: Cursor<Vec<u8>>,
    sink: Vec<u8>,
}

impl FakeStream {
    fn new(resp: &str) -> FakeStream {
        FakeStream {
            resp: Cursor::new(resp.as_bytes().to_vec()),
            sink: Vec::new(),
        }
    }
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

/// Round-trip a canned response through the real client and hand back the
/// parse result. `read_chunked` is private, so the public `send` path is how an
/// integration test reaches it — which is also the path the daemon uses.
fn parse(resp: &str) -> io::Result<Response> {
    let mut stream = FakeStream::new(resp);
    send(&mut stream, "peer", "POST", "/mcp", &[], b"{}")
}

/// The abort: a chunk header declaring a size near `usize::MAX`. The old
/// accumulated-length check computed `body.len() + size`, which wraps — so this
/// panicked "attempt to add with overflow" under the debug overflow-checks this
/// test runs with, and in a release daemon wrapped below the cap and aborted on
/// the multi-exabyte `vec![0u8; size]` that followed. A panic here fails the
/// test outright, which is the assertion: reaching the `unwrap_err` at all
/// proves the parser survived and merely rejected the response.
#[test]
fn chunk_size_near_usize_max_is_rejected_not_overflowed() {
    // The wrap needs `body.len() + size` to exceed `usize::MAX`, so the size has
    // to sit within the already-accumulated 5 bytes of the top — a size merely
    // "near" the max still adds cleanly and would test nothing.
    let huge = format!("{:x}", usize::MAX - 1);
    let resp =
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n{huge}\r\n");
    let err = parse(&resp).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    assert!(
        err.to_string().contains("cap"),
        "expected the cap rejection, got {err}"
    );
}

/// The same header with no preceding chunk — `usize::MAX` does not wrap against
/// an empty body, so this always reached the cap check, but it MUST still never
/// reach an allocation sized from the peer's claim.
#[test]
fn lone_usize_max_chunk_size_is_rejected() {
    let huge = format!("{:x}", usize::MAX);
    let resp = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{huge}\r\n");
    let err = parse(&resp).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

/// An ordinary over-cap claim (9 MiB against the 8 MiB `MAX_RESPONSE`): no
/// arithmetic hazard, but it pins the cap itself so the overflow fix cannot be
/// "fixed" into letting large bodies through.
#[test]
fn chunk_size_over_the_cap_is_rejected() {
    let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n900000\r\n";
    let err = parse(resp).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
}

/// A chunk header that lies *small* — under the cap, so it passes every size
/// check, but the bytes never arrive. Reading incrementally means a short read
/// no longer errors on its own, so the length is re-checked after the fact:
/// a truncated response must not parse as a successful shorter one.
#[test]
fn chunk_shorter_than_its_declared_size_is_rejected() {
    let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n100\r\nonly-a-few\r\n";
    let err = parse(resp).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
}

/// CR/LF in the request TARGET. The crate already closes header injection at
/// the framing layer; a templated endpoint path or a peer-supplied A2A
/// push-notification URL is just as caller-supplied, and splitting the request
/// line lets the injected `Authorization` shadow the operator's real one.
#[test]
fn cr_lf_in_request_target_is_refused_before_anything_is_written() {
    let mut stream = FakeStream::new("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let err = send(
        &mut stream,
        "peer",
        "POST",
        "/mcp\r\nAuthorization: Bearer attacker\r\nX: ",
        &[("Authorization", "Bearer operator-secret")],
        b"{}",
    )
    .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    // Nothing may hit the wire: a half-written poisoned request on a keep-alive
    // connection is the injection, error return or not.
    assert!(
        stream.sink.is_empty(),
        "request bytes escaped: {:?}",
        String::from_utf8_lossy(&stream.sink)
    );
}

/// A bare LF alone splits the line for a permissive server, so the scan cannot
/// be a `\r\n` substring match.
#[test]
fn lone_lf_in_request_target_is_refused() {
    let mut stream = FakeStream::new("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let err = send(&mut stream, "peer", "GET", "/a\nX: 1", &[], b"").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

/// The happy path the hardening must not cost: multi-chunk bodies, a chunk
/// extension on the size line, and trailers after the terminating `0`.
#[test]
fn well_formed_chunked_body_still_parses() {
    let resp = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                5\r\nhello\r\n6;ext=1\r\n world\r\n0\r\nX-Trailer: t\r\n\r\n";
    let parsed = parse(resp).unwrap();
    assert_eq!(parsed.status, 200);
    assert_eq!(parsed.body, b"hello world");
}

/// A body right at the cap boundary — the largest legitimate response — must
/// still parse, proving the `size > MAX_RESPONSE` short-circuit is `>` and not
/// `>=`.
#[test]
fn chunked_body_exactly_at_the_cap_still_parses() {
    let n = net::http::MAX_RESPONSE;
    let mut resp = format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{n:x}\r\n");
    resp.push_str(&"a".repeat(n));
    resp.push_str("\r\n0\r\n\r\n");
    let parsed = parse(&resp).unwrap();
    assert_eq!(parsed.body.len(), n);
}

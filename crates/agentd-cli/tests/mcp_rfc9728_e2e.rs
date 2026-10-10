// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 9728: **issuer discovery from an MCP server's 401 challenge**. A mock
//! MCP endpoint answers an unauthenticated request with
//! `401 WWW-Authenticate: Bearer resource_metadata="…"`; that metadata document
//! lists `authorization_servers`. `agentd login mcp:<name>` with no configured
//! `issuer` walks this chain and learns the authorization server. [feature: oauth]
#![cfg(feature = "oauth")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use agentd::auth::challenge::discover_issuer;

/// A mock resource server: `POST` anything → 401 with the RFC 9728 challenge;
/// `GET /.well-known/oauth-protected-resource` → the metadata JSON naming the
/// issuer. Serves a bounded number of connections then exits.
fn spawn_resource(issuer: &str) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let base = format!("127.0.0.1:{}", addr.port());
    let endpoint = format!("http://{base}/mcp");
    let self_meta = format!("http://{base}/.well-known/oauth-protected-resource");
    let issuer = issuer.to_string();
    std::thread::spawn(move || {
        for c in l.incoming().flatten().take(4) {
            let mut s = c;
            let (method, path) = read_request_line(&mut s);
            if method == "GET" && path.contains("oauth-protected-resource") {
                let body = format!(
                    r#"{{"resource":"http://{base}/mcp","authorization_servers":["{issuer}"]}}"#
                );
                write_resp(&mut s, 200, "OK", &[], body.as_bytes());
            } else {
                // Unauthenticated MCP request → the 401 challenge (RFC 9728 §5.1).
                let wa =
                    format!(r#"Bearer resource_metadata="{self_meta}", error="invalid_token""#);
                write_resp(
                    &mut s,
                    401,
                    "Unauthorized",
                    &[("WWW-Authenticate", &wa)],
                    b"",
                );
            }
        }
    });
    endpoint
}

fn read_request_line(s: &mut TcpStream) -> (String, String) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut start = String::new();
    r.read_line(&mut start).ok();
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    // Drain headers + any body so the client's write completes cleanly.
    let mut clen = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            clen = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; clen];
    let _ = r.read_exact(&mut body);
    (method, path)
}

fn write_resp(s: &mut TcpStream, status: u16, reason: &str, headers: &[(&str, &str)], body: &[u8]) {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

#[test]
fn login_discovers_the_issuer_from_the_401_challenge() {
    let issuer = "http://issuer.example/realms/agents";
    let endpoint = spawn_resource(issuer);

    let found = discover_issuer(&endpoint, Duration::from_secs(5));
    assert_eq!(
        found.as_deref(),
        Some(issuer),
        "the 401 challenge → resource metadata → authorization_servers[0] chain resolves the issuer"
    );
}

/// One request: method, path, lower-cased headers, body.
fn read_request(s: &mut TcpStream) -> (String, String, Vec<(String, String)>, Vec<u8>) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut start = String::new();
    r.read_line(&mut start).ok();
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let clen = headers
        .iter()
        .find(|(k, _)| k == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; clen];
    let _ = r.read_exact(&mut body);
    (method, path, headers, body)
}

/// A resource whose challenge names a same-origin metadata document, and
/// whose well-known URL answers 404 — so only the challenge finds the issuer.
/// `before_auth` decides, from a POST's headers and body, whether the server
/// refuses it (400) before it gets to authentication.
fn spawn_strict(
    issuer: &str,
    before_auth: fn(&[(String, String)], &serde_json::Value) -> bool,
) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("127.0.0.1:{}", l.local_addr().unwrap().port());
    let endpoint = format!("http://{base}/mcp");
    let meta = format!("http://{base}/meta");
    let issuer = issuer.to_string();
    std::thread::spawn(move || {
        for c in l.incoming().flatten().take(8) {
            let mut s = c;
            let (method, path, headers, body) = read_request(&mut s);
            if method == "GET" && path == "/meta" {
                let body = format!(
                    r#"{{"resource":"http://{base}/mcp","authorization_servers":["{issuer}"]}}"#
                );
                write_resp(&mut s, 200, "OK", &[], body.as_bytes());
            } else if method == "GET" {
                write_resp(&mut s, 404, "Not Found", &[], b"");
            } else {
                let msg: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                if before_auth(&headers, &msg) {
                    let refusal = br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32020,"message":"header mismatch"}}"#;
                    write_resp(&mut s, 400, "Bad Request", &[], refusal);
                } else {
                    let wa = format!(r#"Bearer resource_metadata="{meta}""#);
                    write_resp(
                        &mut s,
                        401,
                        "Unauthorized",
                        &[("WWW-Authenticate", &wa)],
                        b"",
                    );
                }
            }
        }
    });
    endpoint
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

#[test]
fn a_server_that_checks_the_revision_before_auth_still_gives_its_challenge() {
    // A 2025-11-25 server: a revision it does not speak is refused before
    // anything else, so only a request without one reaches its challenge.
    let ep = spawn_strict("https://as.example", |headers, _| {
        header(headers, "mcp-protocol-version").is_some_and(|v| v != "2025-11-25")
    });
    assert_eq!(
        discover_issuer(&ep, Duration::from_secs(5)).as_deref(),
        Some("https://as.example")
    );
}

#[test]
fn a_server_that_checks_the_standard_headers_before_auth_still_gives_its_challenge() {
    // A 2026-07-28 server: a request without the standard headers, or whose
    // `Mcp-Method` is not its method, is refused before authentication.
    let ep = spawn_strict("https://as.example", |headers, msg| {
        let meta = &msg["params"]["_meta"];
        header(headers, "mcp-protocol-version").is_none()
            || header(headers, "mcp-method") != msg["method"].as_str()
            || meta["io.modelcontextprotocol/protocolVersion"].as_str()
                != header(headers, "mcp-protocol-version")
            || meta["io.modelcontextprotocol/clientCapabilities"].is_null()
    });
    assert_eq!(
        discover_issuer(&ep, Duration::from_secs(5)).as_deref(),
        Some("https://as.example")
    );
}

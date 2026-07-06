// SPDX-License-Identifier: Apache-2.0
//! Composability E2E over the target-vision HTTP transport (pivot Phase 2): a
//! peer drives agentd's served self-MCP over Streamable HTTP. Runs only under
//! `cargo test --features serve-https`. Uses loopback plaintext (`http://`) so
//! the transport wiring is exercised end-to-end through the real binary without
//! cert plumbing (the TLS acceptor + mTLS are covered by net's tls_server tests
//! and the AgentdHttpAuth unit tests).
#![cfg(all(unix, feature = "serve-https"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn sigterm(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// Grab a free loopback port by binding :0 then dropping the listener. A small
/// TOCTOU window, but agentd rebinds it within milliseconds.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One HTTP POST /mcp with a JSON-RPC body; returns the (status_line, headers,
/// body) once the connection closes.
fn post(
    addr: &str,
    extra_headers: &[(&str, &str)],
    body: &str,
) -> (String, Vec<(String, String)>, String) {
    let mut s = TcpStream::connect(addr).expect("connect served http");
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut head = format!(
        "POST /mcp HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
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
    let mut b = String::new();
    reader.read_to_string(&mut b).unwrap();
    (status, headers, b)
}

/// Connect to the served HTTP port once it accepts (poll up to ~3s).
fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "served http never became connectable"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn spawn_daemon(port: u16, extra: &[&str]) -> Child {
    let exe = env!("CARGO_BIN_EXE_agentd");
    let bind = format!("http://127.0.0.1:{port}");
    let mut args = vec![
        "--mode",
        "loop",
        "--interval",
        "10s",
        "--instruction",
        "x",
        "--intelligence",
        "http://127.0.0.1:9",
        "--serve-mcp",
        &bind,
        "--log-level",
        "warn",
    ];
    args.extend_from_slice(extra);
    Command::new(exe)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agentd loop --serve-mcp http")
}

#[test]
fn a_peer_initializes_lists_and_calls_status_over_http() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let mut child = spawn_daemon(port, &[]);
    wait_ready(&addr);

    // initialize → serverInfo + capabilities, and a session id header (legacy).
    let (status, headers, body) = post(
        &addr,
        &[],
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{}}}"#,
    );
    assert!(status.contains("200"), "init status: {status}");
    assert!(
        headers.iter().any(|(k, _)| k == "mcp-session-id"),
        "initialize must stamp a session id: {headers:?}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["result"]["serverInfo"]["name"], "agentd",
        "init body: {body}"
    );

    // tools/list advertises `status` (one request per connection, Connection: close).
    let (_s, _h, body) = post(
        &addr,
        &[],
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["result"]["tools"][0]["name"], "status",
        "tools/list: {body}"
    );

    // tools/call status returns this daemon's live state.
    let (_s, _h, body) = post(
        &addr,
        &[],
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status"}}"#,
    );
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["result"]["isError"], false, "status: {body}");
    assert_eq!(v["result"]["structuredContent"]["mode"], "loop");

    sigterm(child.id());
    let _ = child.wait();
}

#[test]
fn bearer_auth_gates_the_http_control_plane() {
    // A loopback daemon WITH a bearer must refuse an unauthenticated request
    // (401) and accept the correct token — the mTLS/bearer gate over HTTP.
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let mut child = spawn_daemon(port, &["--serve-bearer", "s3cret-token"]);
    wait_ready(&addr);

    let (status, _h, _b) = post(
        &addr,
        &[],
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    );
    assert!(status.contains("401"), "unauth must be 401: {status}");

    let (status, _h, body) = post(
        &addr,
        &[("Authorization", "Bearer s3cret-token")],
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
    );
    assert!(status.contains("200"), "authed status: {status}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        v["result"]["tools"][0]["name"], "status",
        "authed body: {body}"
    );

    sigterm(child.id());
    let _ = child.wait();
}

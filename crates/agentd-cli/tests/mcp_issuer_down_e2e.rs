// SPDX-License-Identifier: AGPL-3.0-only
//! An MCP server behind a device login whose issuer does not answer: the
//! connection builds and handshakes on the cached token, dialing no issuer;
//! once the token needs renewing a tool call fails — unsent, naming the
//! discovery that failed — and when the issuer answers again the next call is
//! renewed and signed, on the same connection. A proxy in front of the server
//! records every request that reached it. One test in this binary: it sets the
//! credential directory in the process environment.
#![cfg(all(unix, feature = "oauth"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentd::auth::cache::{self, CachedCred};
use agentd::mcp::mock_http::Options;
use serde_json::json;

/// One request as it reached the server: its `Authorization` header and its
/// body.
type Seen = Arc<Mutex<Vec<(Option<String>, String)>>>;

/// One request read off a connection.
struct Request {
    /// The request line and headers, as they arrived.
    head: Vec<u8>,
    body: Vec<u8>,
    auth: Option<String>,
}

/// Read one request off `conn`.
fn read_request(conn: &TcpStream) -> Option<Request> {
    conn.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let mut r = BufReader::new(conn.try_clone().ok()?);
    let mut line = String::new();
    if r.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut head = line.into_bytes();
    let (mut auth, mut len) = (None, 0usize);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        head.extend_from_slice(h.as_bytes());
        if h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            if k.trim().eq_ignore_ascii_case("authorization") {
                auth = Some(v.trim().to_string());
            }
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).ok();
    Some(Request { head, body, auth })
}

/// A proxy in front of `upstream` that records each request before passing it
/// on.
fn spawn_capture_proxy(upstream: &str) -> (String, Seen) {
    let up = upstream.trim_start_matches("http://").to_string();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let uri = format!("http://{}", l.local_addr().unwrap());
    let seen: Seen = Arc::default();
    let rec = Arc::clone(&seen);
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let (rec, up) = (Arc::clone(&rec), up.clone());
            std::thread::spawn(move || {
                let Some(req) = read_request(&c) else {
                    return;
                };
                rec.lock().unwrap().push((
                    req.auth.clone(),
                    String::from_utf8_lossy(&req.body).into_owned(),
                ));
                let Ok(mut server) = TcpStream::connect(&up) else {
                    return;
                };
                let _ = server
                    .write_all(&req.head)
                    .and_then(|()| server.write_all(&req.body));
                let mut back = c;
                let _ = std::io::copy(&mut server, &mut back);
            });
        }
    });
    (uri, seen)
}

/// An issuer whose discovery answers `503` while `down` is set; its token
/// endpoint renews any refresh token as `tok-2`.
fn spawn_issuer(down: Arc<AtomicBool>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let token_url = format!("{base}/token");
    let issuer = base.clone();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let Some(req) = read_request(&c) else {
                continue;
            };
            let line = String::from_utf8_lossy(&req.head).into_owned();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let (status, payload) = if path.starts_with("/.well-known/") {
                if down.load(Ordering::SeqCst) {
                    ("503 Service Unavailable", String::new())
                } else {
                    (
                        "200 OK",
                        format!(r#"{{"issuer":"{issuer}","token_endpoint":"{token_url}"}}"#),
                    )
                }
            } else {
                (
                    "200 OK",
                    r#"{"access_token":"tok-2","refresh_token":"rt-2","expires_in":3600}"#
                        .to_string(),
                )
            };
            let _ = write!(
                c,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    base
}

#[test]
fn a_tool_call_whose_token_cannot_be_renewed_fails_unsent_until_the_issuer_answers() {
    let creds = tempfile::tempdir().unwrap();
    // SAFETY: the one test in this binary, before it starts a thread.
    unsafe { std::env::set_var("AGENTD_CRED_DIR", creds.path()) };
    // A login good for three more seconds before its refresh point (a minute
    // ahead of expiry), with a refresh token.
    let refresh_at = cache::now_ms() + 3_000;
    cache::store_file(
        creds.path(),
        "mcp:tickets",
        &CachedCred {
            access_token: "tok-1".into(),
            refresh_token: Some("rt-1".into()),
            expires_at_ms: refresh_at + 60_000,
            ..Default::default()
        },
    )
    .unwrap();
    let down = Arc::new(AtomicBool::new(true));
    let issuer = spawn_issuer(Arc::clone(&down));
    let upstream = common::mock_in_thread(Options {
        emit: false,
        hold_listen: false,
        ..Options::new("mock://res")
    });
    let (proxy, seen) = spawn_capture_proxy(&upstream);
    let spec = agentd::config::McpServerSpec {
        name: "tickets".into(),
        endpoint: proxy,
        auth: Some(agentd::config::AuthSpec {
            kind: "oauth2".into(),
            grant: Some("device".into()),
            issuer: Some(issuer),
            client_id: Some("agentd".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let tool_calls = || -> Vec<Option<String>> {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|(_, body)| body.contains("\"tools/call\""))
            .map(|(auth, _)| auth.clone())
            .collect()
    };

    // Built and handshaken while the issuer is down: the cached token needs
    // no discovery, and building the connection dials nothing.
    let mut c = agentd::mcp::from_spec(&spec, Duration::from_secs(10))
        .expect("building the connection dials no issuer");
    c.initialize().expect("the cached token connects");
    c.call_tool("state.list", Some(json!({})))
        .expect("a call on the cached token");

    // Past the refresh point, renewing needs the issuer's endpoints.
    while cache::now_ms() <= refresh_at + 200 {
        std::thread::sleep(Duration::from_millis(50));
    }
    let e = c
        .call_tool("state.list", Some(json!({})))
        .expect_err("a token that cannot be renewed sends no call")
        .to_string();
    assert!(
        e.contains("mcp:tickets") && e.contains("discovery failed"),
        "{e}"
    );
    assert_eq!(tool_calls().len(), 1, "the failed call was not sent");

    // The issuer answers: the next call renews the token and is signed.
    down.store(false, Ordering::SeqCst);
    c.call_tool("state.list", Some(json!({})))
        .expect("renewed, the call goes out");
    assert_eq!(
        tool_calls(),
        vec![
            Some("Bearer tok-1".to_string()),
            Some("Bearer tok-2".to_string())
        ]
    );
    let all = seen.lock().unwrap().clone();
    assert!(
        all.iter().all(|(auth, _)| auth.is_some()),
        "every request that reached the server carried a token: {all:?}"
    );
}

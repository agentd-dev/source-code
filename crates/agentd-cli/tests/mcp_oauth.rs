// SPDX-License-Identifier: Apache-2.0
//! End-to-end test of the OAuth 2.1 client-credentials token source against a
//! mock token endpoint on loopback TCP. Proves the grant POST → parse → cache
//! path: `bearer()` fetches once, then serves the cached token without a second
//! round-trip. [feature: oauth]
#![cfg(feature = "oauth")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use agentd::mcp::oauth::{OAuthClient, OAuthConfig};

/// A mock token endpoint. Returns a JSON token response and records each request
/// body so the test can assert the grant form was posted and caching held.
fn spawn_token_endpoint() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/token");
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let bodies_thread = Arc::clone(&bodies);

    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let mut content_length = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 {
                    break;
                }
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length")
                {
                    content_length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut buf = vec![0u8; content_length];
            reader.read_exact(&mut buf).unwrap();
            bodies_thread
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf).into_owned());

            let payload = br#"{"access_token":"tok-1","token_type":"Bearer","expires_in":3600}"#;
            let mut stream: TcpStream = stream;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(payload);
            let _ = stream.flush();
        }
    });

    (url, bodies)
}

#[test]
fn client_credentials_fetches_then_caches() {
    // SAFETY: single-threaded test; unique var name.
    unsafe { std::env::set_var("MCP_OAUTH_TEST_SECRET", "sh!hh/secret") };
    let (token_url, bodies) = spawn_token_endpoint();

    let client = OAuthClient::new(
        OAuthConfig {
            token_url,
            client_id: "agentd-client".into(),
            client_secret: "{{secret:MCP_OAUTH_TEST_SECRET}}".into(),
            scope: Some("mcp:read mcp:write".into()),
        },
        Duration::from_secs(5),
    );

    // First call fetches.
    assert_eq!(client.bearer().unwrap(), "tok-1");
    // Second call is served from cache — no second round-trip.
    assert_eq!(client.bearer().unwrap(), "tok-1");

    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1, "the token is cached — exactly one fetch");
    let form = &bodies[0];
    assert!(form.contains("grant_type=client_credentials"), "{form}");
    assert!(form.contains("client_id=agentd-client"), "{form}");
    // The resolved secret is form-encoded (special chars escaped), never inline.
    assert!(form.contains("client_secret=sh%21hh%2Fsecret"), "{form}");
    assert!(form.contains("scope=mcp%3Aread%20mcp%3Awrite"), "{form}");

    unsafe { std::env::remove_var("MCP_OAUTH_TEST_SECRET") };
}

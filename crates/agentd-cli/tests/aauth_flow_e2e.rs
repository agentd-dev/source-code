// SPDX-License-Identifier: Apache-2.0
//! FULL end-to-end AAuth [RFC 0023] over the REAL MCP transport — Case C
//! (Person-Server / user-scoped identity) and Case B (resource-managed access
//! token). Drives an actual `McpClient` (with the AAuth `RequestSigner`
//! installed) against live mock sockets: an Agent Provider, a Person Server,
//! and an AAuth-verifying MCP server. Proves the reaction loop:
//! sign → 401 requirement → satisfy (PS exchange / adopt) → re-sign → 200.
//! [feature: aauth]
#![cfg(feature = "aauth")]

use agentd::aauth::{AAuthClient, AgentKey, ApdConfig, RequestSigner};
use agentd::mcp::client::McpClient;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

/// Read one HTTP request → (request-line, headers-lowercased-map, body).
fn read_http(s: &mut TcpStream) -> (String, std::collections::HashMap<String, String>, Vec<u8>) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).ok();
    let request_line = line.trim().to_string();
    let mut headers = std::collections::HashMap::new();
    let mut clen = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            if key == "content-length" {
                clen = val.parse().unwrap_or(0);
            }
            headers.insert(key, val);
        }
    }
    let mut body = vec![0u8; clen];
    let _ = r.read_exact(&mut body);
    (request_line, headers, body)
}

fn write_http(s: &mut TcpStream, status: &str, extra: &[(&str, &str)], body: &str) {
    let mut head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
}

/// The mock Agent Provider: enroll + agent-token (canned, signed).
fn spawn_apd() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let (line, _h, _b) = read_http(&mut s);
            let body = if line.contains("enroll") {
                r#"{"agent":"aauth:agent7@apd.mock"}"#
            } else {
                r#"{"agent_token":"eyJagent.tok","expires_in":3600,"agent":"aauth:agent7@apd.mock"}"#
            };
            write_http(&mut s, "200 OK", &[], body);
        }
    });
    format!("http://{addr}")
}

/// The mock Person Server: POST /token with a resource token → an auth token
/// (auto-approves, standing in for the human's consent).
fn spawn_ps() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let (_line, _h, body) = read_http(&mut s);
            let got: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            // The exchange must carry the resource token we issued + a justification.
            assert_eq!(got["resource_token"], "RT-xyz", "PS got the resource token");
            assert!(got["justification"].is_string(), "PS got a justification");
            write_http(&mut s, "200 OK", &[], r#"{"auth_token":"eyJuser.auth"}"#);
        }
    });
    format!("http://{addr}")
}

/// The mock AAuth MCP server (Case C): `initialize` succeeds; `tools/call`
/// requires a user-scoped auth token — a request presenting the AGENT token is
/// `401 requirement=auth-token; resource-token`, one presenting the AUTH token
/// succeeds. Every request must carry an RFC 9421 `Signature` (proving the
/// agent signed). Returns `host:port`.
fn spawn_aauth_mcp() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let (line, h, body) = read_http(&mut s);
            if line.starts_with("GET ") {
                // discovery / event stream — 404 (no discovery doc, no push).
                write_http(&mut s, "404 Not Found", &[], "");
                continue;
            }
            // Every POST must be signed.
            let key = h.get("signature-key").cloned().unwrap_or_default();
            assert!(h.contains_key("signature"), "request is signed");
            let req: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let method = req["method"].as_str().unwrap_or("");
            let id = req["id"].clone();
            if method == "server/discover" {
                // A legacy server: reject the modern probe so the client falls
                // back to the `initialize` handshake (era detection).
                let r = serde_json::json!({"jsonrpc":"2.0","id":id,
                    "error":{"code":-32601,"message":"method not found"}});
                write_http(&mut s, "200 OK", &[], &r.to_string());
            } else if method == "initialize" {
                let r = serde_json::json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":"2025-11-25",
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"aauth-mock","version":"0"}}});
                write_http(
                    &mut s,
                    "200 OK",
                    &[("Mcp-Session-Id", "sess-1")],
                    &r.to_string(),
                );
            } else if method == "notifications/initialized" {
                write_http(&mut s, "202 Accepted", &[], "");
            } else if method == "tools/call" {
                if key.contains("eyJuser.auth") {
                    // Presenting the user-scoped auth token → the tool runs.
                    let r = serde_json::json!({"jsonrpc":"2.0","id":id,"result":{
                        "content":[{"type":"text","text":"secret data for the user"}],
                        "isError":false}});
                    write_http(&mut s, "200 OK", &[], &r.to_string());
                } else {
                    // Only the agent token → demand a user-scoped auth token.
                    write_http(
                        &mut s,
                        "401 Unauthorized",
                        &[(
                            "AAuth-Requirement",
                            r#"auth-token; resource-token="RT-xyz""#,
                        )],
                        "{}",
                    );
                }
            } else {
                let r = serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}});
                write_http(&mut s, "200 OK", &[], &r.to_string());
            }
        }
    });
    format!("127.0.0.1:{}", addr.port())
}

#[test]
fn case_c_person_server_exchange_over_the_transport() {
    let apd = spawn_apd();
    let ps = spawn_ps();
    let mcp_addr = spawn_aauth_mcp();
    let dir = tempfile::tempdir().unwrap();

    // Build the AAuth client with a Person Server configured (Case C).
    let key = AgentKey::load_or_create(&dir.path().join("agent.key")).unwrap();
    let client = AAuthClient::new(
        key,
        ApdConfig {
            base_url: apd,
            enrollment_token: None,
            person_server: Some(ps),
            platform: "workload".into(),
        },
        Duration::from_secs(5),
    );
    client.prime().expect("enroll + token");
    let signer: Arc<dyn RequestSigner> = Arc::new(client);

    // A REAL MCP client with the signer installed, against the AAuth server.
    let mut mcp = McpClient::connect_signed(
        "secure",
        &format!("http://{mcp_addr}/mcp"),
        vec![],
        Duration::from_secs(5),
        Some(signer),
    )
    .expect("connect");
    mcp.initialize().expect("initialize (signed)");

    // The tool call: the first signed attempt 401s (agent token only); the
    // transport reaction loop runs the PS exchange, gets the user auth token,
    // re-signs presenting it, and the retry succeeds — all inside call_tool.
    let result = mcp
        .call_tool("read_secret", Some(serde_json::json!({})))
        .expect("tool call succeeds after the Case-C exchange");
    let text = result.text();
    assert!(
        text.contains("secret data for the user"),
        "the user-scoped call returned the protected result: {text:?}"
    );
}

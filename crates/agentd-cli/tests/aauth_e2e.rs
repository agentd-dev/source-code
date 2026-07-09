// SPDX-License-Identifier: Apache-2.0
//! End-to-end AAuth [DRAFT] (RFC 0023): the agent-side chain against a LIVE mock
//! Agent Provider socket — load key → enroll (signed) → agent-token (signed,
//! cached) → produce RFC 9421 request-signature headers that a verifier
//! reconstructs and checks against the enrolled public key. Case A, the common
//! path. [feature: aauth]
#![cfg(feature = "aauth")]

use agentd::aauth::{AAuthClient, AgentKey, ApdConfig, RequestSigner};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

/// A minimal mock apd: answers POST /enroll and /agent-token. It records that
/// each request arrived signed (a `Signature` header present) and returns canned
/// JSON. Runs until it has served `serve` requests, then stops.
fn spawn_mock_apd(serve: usize) -> (String, mpsc::Receiver<bool>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..serve {
            let Ok((mut s, _)) = listener.accept() else {
                break;
            };
            let (path, signed) = read_request(&mut s);
            tx.send(signed).ok();
            let body = if path.contains("enroll") {
                r#"{"agent":"aauth:k7q3p9n2@apd.mock"}"#
            } else {
                r#"{"agent_token":"eyJmock.agent.token","expires_in":3600,"agent":"aauth:k7q3p9n2@apd.mock"}"#
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });
    (format!("http://{addr}"), rx)
}

/// Read one HTTP request; return (request-line, whether a `Signature` header was
/// present) — proving the client signed the apd call.
fn read_request(s: &mut TcpStream) -> (String, bool) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).ok();
    let request_line = line.trim().to_string();
    let mut signed = false;
    let mut content_length = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        let lower = t.to_ascii_lowercase();
        if lower.starts_with("signature:") || lower.starts_with("signature-key:") {
            signed = true;
        }
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    let _ = reader.read_exact(&mut body);
    (request_line, signed)
}

#[test]
fn enroll_token_and_sign_end_to_end() {
    let (provider, rx) = spawn_mock_apd(2); // enroll + agent-token
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("agent.key");
    let key = AgentKey::load_or_create(&key_path).expect("key");
    let pubkey = key.public_bytes().to_vec();

    let client = AAuthClient::new(
        key,
        ApdConfig {
            base_url: provider,
            enrollment_token: Some("one-time-abc".into()),
            person_server: None,
            platform: "workload".into(),
        },
        Duration::from_secs(5),
    );

    // Prime: enroll + fetch the first token. Both apd calls arrived SIGNED.
    let agent_id = client.prime().expect("prime enrolls + gets a token");
    assert_eq!(agent_id, "aauth:k7q3p9n2@apd.mock");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(true),
        "enroll signed"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2)),
        Ok(true),
        "token signed"
    );

    // The signer produces the three RFC 9421 headers for an MCP request…
    let hdrs = client.sign("POST", "mcp.example", "/mcp");
    let map: std::collections::HashMap<_, _> = hdrs.into_iter().collect();
    assert!(map.contains_key("Signature-Input"));
    assert!(map.contains_key("Signature"));
    // The presented key id is the agent token (Case A / Step 6).
    assert_eq!(map["Signature-Key"], "sig=jwt;jwt=\"eyJmock.agent.token\"");

    // …and a verifier reconstructs the base and checks the Ed25519 signature
    // against the enrolled public key (what a real AAuth MCP server does).
    let created = map["Signature-Input"]
        .rsplit("created=")
        .next()
        .unwrap()
        .to_string();
    let base = format!(
        "\"@method\": POST\n\
         \"@authority\": mcp.example\n\
         \"@path\": /mcp\n\
         \"signature-key\": {}\n\
         \"@signature-params\": (\"@method\" \"@authority\" \"@path\" \"signature-key\");created={created}",
        map["Signature-Key"]
    );
    let raw = map["Signature"]
        .trim_start_matches("sig=:")
        .trim_end_matches(':');
    let sig = base64_std_decode(raw);
    agentd::aauth::verify_ed25519(&pubkey, base.as_bytes(), &sig)
        .expect("request signature verifies against the enrolled public key");

    // A second sign() reuses the CACHED token — no more apd calls (the mock was
    // told to serve only 2; a third connect would block/fail).
    let again = client.sign("POST", "mcp.example", "/mcp");
    assert_eq!(again.len(), 3);
}

/// Minimal standard-base64 decode (the RFC 9421 signature value is std b64).
fn base64_std_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        let Some(v) = val(c) else { continue };
        bits = bits << 6 | v as u32;
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
        }
    }
    out
}

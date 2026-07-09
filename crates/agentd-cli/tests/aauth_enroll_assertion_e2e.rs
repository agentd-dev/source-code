// SPDX-License-Identifier: Apache-2.0
//! AAuth [RFC 0023 §5.1] federated enrollment — the agent presents an
//! `enrollment_assertion` (e.g. a Kubernetes projected ServiceAccount token) in
//! the `/enroll` body, read **fresh from the file on every enroll** so a rotated
//! projected token is always current. Proven against a live mock Agent Provider
//! that captures the enroll body. [feature: aauth]
#![cfg(feature = "aauth")]

use agentd::aauth::{ApdClient, ApdConfig};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn read_http(s: &mut TcpStream) -> (String, Vec<u8>) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).ok();
    let request_line = line.trim().to_string();
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
        if let Some((k, v)) = t.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            clen = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; clen];
    let _ = r.read_exact(&mut body);
    (request_line, body)
}

fn write_json(s: &mut TcpStream, body: &str) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
}

/// A mock apd that records every `/enroll` body it receives, then answers
/// enroll + agent-token canned.
fn spawn_apd(seen_enroll_bodies: Arc<Mutex<Vec<serde_json::Value>>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let (line, body) = read_http(&mut s);
            if line.contains("enroll") {
                let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                seen_enroll_bodies.lock().unwrap().push(v);
                write_json(&mut s, r#"{"agent":"aauth:agent1@apd.mock"}"#);
            } else {
                write_json(
                    &mut s,
                    r#"{"agent_token":"eyJagent.tok","expires_in":3600}"#,
                );
            }
        }
    });
    format!("http://{addr}")
}

/// Two independent process starts (fresh `ApdClient`s) reading the SAME assertion
/// file across a token rotation: each enroll must carry the file's CURRENT
/// contents — proving the assertion is read at enroll time, never cached at
/// construction.
#[test]
fn enroll_presents_assertion_read_fresh_from_file() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = spawn_apd(seen.clone());
    let dir = tempfile::tempdir().unwrap();
    let assertion_path = dir.path().join("sa-token");
    // Same durable identity across both "starts" (same seed).
    let seed = [7u8; 32];

    let cfg = |p: &std::path::Path| ApdConfig {
        base_url: provider.clone(),
        enrollment_token: None,
        enroll_assertion_file: Some(p.to_str().unwrap().to_string()),
        person_server: None,
        platform: "workload".into(),
    };

    // First "pod start": the projected token is V1.
    std::fs::write(&assertion_path, "PROJECTED-SA-TOKEN-V1\n").unwrap();
    let key1 = agentd::aauth::AgentKey::from_seed(&seed).unwrap();
    let c1 = ApdClient::new(cfg(&assertion_path), key1, Duration::from_secs(5));
    c1.token().expect("enroll + token (v1)");

    // The token rotates on disk; a fresh process (new client) reads V2.
    std::fs::write(&assertion_path, "PROJECTED-SA-TOKEN-V2\n").unwrap();
    let key2 = agentd::aauth::AgentKey::from_seed(&seed).unwrap();
    let c2 = ApdClient::new(cfg(&assertion_path), key2, Duration::from_secs(5));
    c2.token().expect("enroll + token (v2)");

    let bodies = seen.lock().unwrap();
    assert_eq!(bodies.len(), 2, "each client enrolled once");
    assert_eq!(
        bodies[0]["enrollment_assertion"], "PROJECTED-SA-TOKEN-V1",
        "first enroll carried the file's contents at that time (whitespace trimmed)"
    );
    assert_eq!(
        bodies[1]["enrollment_assertion"], "PROJECTED-SA-TOKEN-V2",
        "the second enroll re-read the rotated token — not a construction-time cache"
    );
    // The platform hint rides along; no enrollment_token in federated mode.
    assert_eq!(bodies[0]["platform"], "workload");
    assert!(bodies[0].get("enrollment_token").is_none());
}

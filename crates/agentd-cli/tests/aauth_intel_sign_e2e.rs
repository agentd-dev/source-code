// SPDX-License-Identifier: Apache-2.0
//! AAuth [RFC 0023; agentctl RFC 0024 §7.1] — the **intelligence dial is
//! signed**. When a process AAuth identity is installed, agentd's LLM client
//! carries RFC 9421 signature headers on every model call, so a modelgateway can
//! attest the agent by signature instead of source IP. Proven end to end: a real
//! `IntelClient` (with the process signer installed) dials a mock LLM that
//! captures and inspects the request headers. [feature: aauth]
#![cfg(feature = "aauth")]

use agentd::aauth::{AAuthClient, AgentKey, ApdConfig};
use agentd::intel::client::IntelClient;
use agentd::wire::intel::{Message, Request};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn read_http(s: &mut TcpStream) -> (String, HashMap<String, String>, Vec<u8>) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).ok();
    let request_line = line.trim().to_string();
    let mut headers = HashMap::new();
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

fn write_json(s: &mut TcpStream, body: &str) {
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body.as_bytes());
}

/// Mock Agent Provider: canned enroll + agent-token.
fn spawn_apd() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let (line, _h, _b) = read_http(&mut s);
            let body = if line.contains("enroll") {
                r#"{"agent":"aauth:agent9@apd.mock"}"#
            } else {
                r#"{"agent_token":"eyJagent.tok","expires_in":3600,"agent":"aauth:agent9@apd.mock"}"#
            };
            write_json(&mut s, body);
        }
    });
    format!("http://{addr}")
}

/// Mock OpenAI-compatible LLM: captures the FIRST request's headers, then answers
/// a canned chat completion. Returns `host:port`.
fn spawn_llm(captured: Arc<Mutex<Option<HashMap<String, String>>>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let (_line, h, _b) = read_http(&mut s);
            {
                let mut slot = captured.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(h);
                }
            }
            write_json(
                &mut s,
                r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            );
        }
    });
    format!("127.0.0.1:{}", addr.port())
}

#[test]
fn intel_dial_is_aauth_signed_when_identity_installed() {
    let apd = spawn_apd();
    let captured: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
    let llm = spawn_llm(captured.clone());
    let dir = tempfile::tempdir().unwrap();

    // Install a process AAuth identity (primed against the mock apd).
    let key = AgentKey::load_or_create(&dir.path().join("agent.key")).unwrap();
    let client = AAuthClient::new(
        key,
        ApdConfig {
            base_url: apd,
            enrollment_token: None,
            enroll_assertion_file: None,
            person_server: None,
            platform: "workload".into(),
        },
        Duration::from_secs(5),
    );
    client.prime().expect("enroll + token");
    agentd::aauth::install(client);

    // A real intel client with a static bearer, dialing the mock LLM.
    let intel =
        IntelClient::from_parts(&format!("http://{llm}"), Some("sk-bearer".into())).unwrap();
    let req = Request {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        max_tokens: 16,
        temperature: Some(0.0),
    };
    intel.complete(&req).expect("completion succeeds");

    let headers = captured
        .lock()
        .unwrap()
        .take()
        .expect("the LLM received a request");
    // The three RFC 9421 headers are present on the model dial.
    assert!(
        headers.contains_key("signature-input"),
        "signed: {headers:?}"
    );
    assert!(headers.contains_key("signature"), "signed: {headers:?}");
    let sk = headers.get("signature-key").expect("signature-key present");
    assert!(
        sk.contains("eyJagent.tok"),
        "presents the agent token as the key id: {sk}"
    );
    // The static bearer still rides alongside — signing is additive.
    assert_eq!(
        headers.get("authorization").map(String::as_str),
        Some("Bearer sk-bearer"),
    );
}

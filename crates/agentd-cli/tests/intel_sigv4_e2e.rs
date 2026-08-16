// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0031: an `intelligence.auth: { kind: aws }` **SigV4-signs the LLM dial**.
//! A real `IntelClient` with an AWS SigV4 signer dials a mock endpoint that
//! captures the request headers; the `Authorization` is an `AWS4-HMAC-SHA256`
//! signature (Bedrock / an AWS-IAM-gated gateway authenticates by SigV4, not a
//! bearer). [feature: oauth]
#![cfg(feature = "oauth")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentd::intel::client::IntelClient;
use agentd::wire::intel::{Message, Request};

fn read_http(s: &mut TcpStream) -> HashMap<String, String> {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).ok();
    let mut headers = HashMap::new();
    let mut clen = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            if key == "content-length" {
                clen = v.trim().parse().unwrap_or(0);
            }
            headers.insert(key, v.trim().to_string());
        }
    }
    let mut body = vec![0u8; clen];
    let _ = r.read_exact(&mut body);
    headers
}

fn spawn_llm(captured: Arc<Mutex<Option<HashMap<String, String>>>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let h = read_http(&mut s);
            {
                let mut slot = captured.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(h);
                }
            }
            let body = r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(body.as_bytes());
        }
    });
    format!("127.0.0.1:{port}")
}

#[test]
fn intelligence_dial_is_sigv4_signed_for_aws_auth() {
    // SAFETY: this binary has one test; env is set then read on this thread.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
        std::env::set_var(
            "AWS_SECRET_ACCESS_KEY",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        );
    }
    let captured: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
    let llm = spawn_llm(captured.clone());

    let authspec = agentd::config::AuthSpec {
        kind: "aws".into(),
        region: Some("us-east-1".into()),
        service: Some("bedrock".into()),
        source: Some("env".into()),
        ..Default::default()
    };
    let signer = agentd::auth::aws::SigV4Signer::from_spec(&authspec, "intelligence").unwrap();
    let intel = IntelClient::from_parts(&format!("http://{llm}"), None)
        .unwrap()
        .with_signer(Some(signer as Arc<dyn agentd::mcp::http::RequestSigner>));
    let req = Request {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        max_tokens: 8,
        temperature: Some(0.0),
    };
    intel.complete(&req).expect("completion succeeds");

    let h = captured
        .lock()
        .unwrap()
        .take()
        .expect("the LLM got a request");
    let auth = h.get("authorization").expect("Authorization header");
    assert!(
        auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
        "the LLM dial is SigV4-signed: {auth}"
    );
    assert!(
        auth.contains("/us-east-1/bedrock/aws4_request"),
        "region+service scope: {auth}"
    );
    assert!(h.contains_key("x-amz-date"), "x-amz-date present: {h:?}");

    unsafe {
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }
}

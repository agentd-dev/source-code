// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0031 §8: **native Amazon Bedrock**. An `intelligence.dialect: bedrock`
//! client dials the mock Bedrock runtime end-to-end and we assert the full
//! contract:
//!   * the request-target is the dynamic `/model/{modelId}/converse` with the
//!     model id URI-encoded (`:` → `%3A`) — the SAME string that is SigV4-signed;
//!   * the request is SigV4-signed (`Authorization: AWS4-HMAC-SHA256 …`), not a
//!     bearer;
//!   * the body is Converse-shaped (`inferenceConfig`, no top-level `model`);
//!   * a Converse response parses back into the neutral `Response`.
//!
//! [feature: oauth]
#![cfg(feature = "oauth")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentd::intel::client::IntelClient;
use agentd::wire::intel::{Message, Request, ToolDef};
use serde_json::Value;

/// What the mock captured from the one request it served.
#[derive(Default)]
struct Captured {
    method: String,
    path: String,
    authorization: String,
    body: Vec<u8>,
}

fn read_request(s: &mut TcpStream) -> Captured {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut start = String::new();
    r.read_line(&mut start).ok();
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut authorization = String::new();
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
            if key == "authorization" {
                authorization = v.trim().to_string();
            }
        }
    }
    let mut body = vec![0u8; clen];
    let _ = r.read_exact(&mut body);
    Captured {
        method,
        path,
        authorization,
        body,
    }
}

fn spawn_bedrock(captured: Arc<Mutex<Option<Captured>>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let mut s = c;
            let req = read_request(&mut s);
            {
                let mut slot = captured.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(req);
                }
            }
            // A Bedrock Converse-shaped success reply.
            let body = r#"{"output":{"message":{"role":"assistant","content":[{"text":"bedrock says hi"}]}},"stopReason":"end_turn","usage":{"inputTokens":11,"outputTokens":3,"totalTokens":14}}"#;
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
fn native_bedrock_dialect_signs_the_dynamic_path_and_round_trips() {
    // SAFETY: this binary has one test; env is set then read on this thread.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
        std::env::set_var(
            "AWS_SECRET_ACCESS_KEY",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        );
    }
    let captured: Arc<Mutex<Option<Captured>>> = Arc::new(Mutex::new(None));
    let host = spawn_bedrock(captured.clone());

    let authspec = agentd::config::AuthSpec {
        kind: "aws".into(),
        region: Some("us-east-1".into()),
        service: Some("bedrock".into()),
        source: Some("env".into()),
        ..Default::default()
    };
    let signer = agentd::auth::aws::SigV4Signer::from_spec(&authspec, "intelligence").unwrap();
    let intel = IntelClient::from_parts(&format!("http://{host}"), None)
        .unwrap()
        .with_dialect(Some("bedrock"))
        .with_signer(Some(signer as Arc<dyn agentd::mcp::http::RequestSigner>));

    let model = "anthropic.claude-3-5-sonnet-20241022-v2:0";
    let req = Request {
        model: model.into(),
        messages: vec![Message::system("be terse"), Message::user("hi")],
        tools: vec![ToolDef {
            name: "read_file".into(),
            description: "read".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        max_tokens: 32,
        temperature: Some(0.0),
    };
    let resp = intel.complete(&req).expect("completion succeeds");

    // The neutral response parsed from the Converse reply.
    assert_eq!(resp.text.as_deref(), Some("bedrock says hi"));
    assert_eq!(resp.usage.total(), 14);

    let cap = captured
        .lock()
        .unwrap()
        .take()
        .expect("mock served a request");
    // Dynamic path: /model/{URI-encoded model id}/converse. The `:` is %3A.
    assert_eq!(cap.method, "POST");
    assert_eq!(
        cap.path, "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse",
        "the model id rides the encoded path"
    );
    // SigV4-signed (not a bearer), and the credential scope names the encoded path.
    assert!(
        cap.authorization
            .starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"),
        "Bedrock is SigV4-signed: {}",
        cap.authorization
    );
    assert!(
        cap.authorization
            .contains("/us-east-1/bedrock/aws4_request"),
        "region+service scope: {}",
        cap.authorization
    );
    // Converse-shaped body: inferenceConfig present, NO top-level model.
    let body: Value = serde_json::from_slice(&cap.body).expect("body is JSON");
    assert!(body.get("model").is_none(), "no top-level model: {body}");
    assert_eq!(body["inferenceConfig"]["maxTokens"], 32);
    assert_eq!(body["system"][0]["text"], "be terse");
    assert_eq!(body["messages"][0]["content"][0]["text"], "hi");
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "read_file"
    );

    unsafe {
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }
}

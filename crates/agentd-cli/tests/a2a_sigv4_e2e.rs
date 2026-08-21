// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0031 §8 (deferred follow-up): **outbound A2A SigV4**. A peer with
//! `auth: { kind: aws }` is dialed with a per-request SigV4 signature over the
//! exact JSON-RPC body — so an AWS-IAM-gated peer authenticates the caller by
//! signature, not a bearer. The mock peer captures the request head and we assert
//! the `Authorization: AWS4-HMAC-SHA256 …` + `X-Amz-Date`. [features: a2a, oauth]
#![cfg(all(feature = "a2a", feature = "oauth"))]

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::config::A2aEndpoint;
use agentd::mcp::a2a_client::{DelegateOutcome, PeerAuth, delegate};

/// A mock A2A peer: captures the first request's head, then replies with a
/// unary (`application/json`) terminal COMPLETED Task carrying a distillate — so
/// `delegate` finishes after one signed request.
fn spawn_peer(captured: Arc<Mutex<String>>) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Some(Ok(mut s)) = l.incoming().next() {
            s.set_read_timeout(Some(Duration::from_secs(3))).ok();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut clen = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = line.split_once(':')
                    && k.trim().eq_ignore_ascii_case("content-length")
                {
                    clen = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
            }
            let mut body = vec![0u8; clen];
            let _ = r.read_exact(&mut body);
            *captured.lock().unwrap() = head;

            let task = r#"{"id":"t-1","contextId":"ctx","status":{"state":"TASK_STATE_COMPLETED","timestamp":"1970-01-01T00:00:00.000Z"},"artifacts":[{"artifactId":"t-1.d","parts":[{"text":"signed ok"}]}]}"#;
            let payload = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{task}}}"#);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    format!("http://{addr}")
}

#[test]
fn outbound_a2a_is_sigv4_signed_for_aws_peer_auth() {
    // SAFETY: this binary has one test; env is set then read on this thread.
    unsafe {
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE");
        std::env::set_var(
            "AWS_SECRET_ACCESS_KEY",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        );
    }
    let captured: Arc<Mutex<String>> = Arc::default();
    let url = spawn_peer(Arc::clone(&captured));
    let ep = A2aEndpoint::parse(&url).unwrap();

    let authspec = agentd::config::AuthSpec {
        kind: "aws".into(),
        region: Some("us-east-1".into()),
        service: Some("execute-api".into()),
        source: Some("env".into()),
        ..Default::default()
    };
    let signer = agentd::auth::aws::SigV4Signer::from_spec(&authspec, "a2a:peer").unwrap();
    let auth = PeerAuth {
        signer: Some(signer as Arc<dyn agentd::mcp::http::RequestSigner>),
        ..Default::default()
    };

    let deadline = Instant::now() + Duration::from_secs(5);
    match delegate(&ep, auth, "do the work", None, None, deadline) {
        DelegateOutcome::Distillate(s) => assert_eq!(s, "signed ok"),
        DelegateOutcome::Error(e) => panic!("unexpected error: {e}"),
    }

    let head = captured.lock().unwrap().to_lowercase();
    assert!(
        head.contains("authorization: aws4-hmac-sha256 credential=akidexample/"),
        "the A2A dial is SigV4-signed:\n{head}"
    );
    assert!(
        head.contains("/us-east-1/execute-api/aws4_request"),
        "region+service scope on the wire:\n{head}"
    );
    assert!(head.contains("x-amz-date:"), "x-amz-date present:\n{head}");

    unsafe {
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
    }
}

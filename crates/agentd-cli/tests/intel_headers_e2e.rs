// SPDX-License-Identifier: AGPL-3.0-only
//! The configured `intelligence.headers` (and a device-login bearer) reach the
//! LLM wire. A real `IntelClient` with `with_headers(...)` dials a mock
//! OpenAI-compatible endpoint that captures the request headers; the configured
//! headers and the bearer are all present on the model call.

use agentd::intel::client::IntelClient;
use agentd::wire::intel::{Message, Request};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn read_http(s: &mut TcpStream) -> HashMap<String, String> {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    r.read_line(&mut line).ok(); // request line
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
fn intelligence_headers_and_bearer_reach_the_llm() {
    let captured: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
    let llm = spawn_llm(captured.clone());

    let intel = IntelClient::from_parts(&format!("http://{llm}"), Some("sk-tok".into()))
        .unwrap()
        .with_headers(vec![
            ("X-Team".into(), "ops".into()),
            ("X-Route".into(), "eu".into()),
        ]);
    let req = Request {
        model: "m".into(),
        messages: vec![Message::user("hi")],
        tools: Vec::new(),
        max_tokens: 16,
        temperature: Some(0.0),
        decision: None,
        reasoning: None,
        output_schema: None,
    };
    intel.complete(&req).expect("completion succeeds");

    let h = captured
        .lock()
        .unwrap()
        .take()
        .expect("the LLM received a request");
    // The configured intelligence.headers ride the dial…
    assert_eq!(h.get("x-team").map(String::as_str), Some("ops"), "{h:?}");
    assert_eq!(h.get("x-route").map(String::as_str), Some("eu"), "{h:?}");
    // …alongside the bearer (the dialect's Authorization header).
    assert_eq!(
        h.get("authorization").map(String::as_str),
        Some("Bearer sk-tok"),
        "{h:?}"
    );
}

/// A turn runs in a worker process of its own: the operator's
/// `http.user_agent` reaches the model call it makes, rendered by the
/// supervisor and handed down.
#[test]
fn a_turn_workers_model_call_carries_the_operators_user_agent() {
    let captured: Arc<Mutex<Option<HashMap<String, String>>>> = Arc::new(Mutex::new(None));
    let llm = spawn_llm(captured.clone());
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("agent.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: ua-check\n  instruction: Say ok.\n  prompt: Say ok.\n  preflight: never\n\
             intelligence:\n  endpoints: http://{llm}\n  model: m\n\
             store:\n  kind: memory\n\
             http:\n  user_agent: \"acme-ops/{{{{version}}}} ({{{{os}}}})\"\n\
             lifecycle:\n  run_until: drained\n"
        ),
    )
    .unwrap();
    let log = dir.path().join("agentd.log");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .expect("agentd runs");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while captured.lock().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    let h = captured.lock().unwrap().take().unwrap_or_else(|| {
        panic!(
            "the model was called:\n{}",
            std::fs::read_to_string(&log).unwrap_or_default()
        )
    });
    assert_eq!(
        h.get("user-agent").map(String::as_str),
        Some(
            format!(
                "acme-ops/{} ({})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS
            )
            .as_str()
        ),
        "{h:?}"
    );
}

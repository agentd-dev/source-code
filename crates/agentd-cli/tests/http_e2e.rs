// SPDX-License-Identifier: AGPL-3.0-only
//! agentd 2.0 **outbound `http` node** (RFC 0027) end to end: a workflow makes a
//! real `POST` to a loopback REST endpoint (`allow_private: true`), the SSRF-
//! guarded client sends the templated JSON body + headers, and the structured
//! response `{status, ok, headers, body, json}` flows into the next step's data
//! and out through the run's captured output.
#![cfg(unix)]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// One request the mock REST server received.
#[derive(Clone, Default)]
struct Received {
    method: String,
    path: String,
    content_type: String,
    x_test: String,
    signature: String,
    body: String,
}

/// A loopback REST endpoint that records the request it receives and replies
/// `200` with a distinctive JSON body. Returns `(port, shared-slot, handle)`.
fn spawn_mock_rest() -> (
    u16,
    Arc<Mutex<Option<Received>>>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let slot: Arc<Mutex<Option<Received>>> = Arc::new(Mutex::new(None));
    let seen = slot.clone();
    let handle = std::thread::spawn(move || {
        // Serve a bounded number of connections, then retire.
        for _ in 0..4 {
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            handle_conn(stream, &seen);
        }
    });
    (port, slot, handle)
}

fn handle_conn(stream: TcpStream, seen: &Arc<Mutex<Option<Received>>>) {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut reader = BufReader::new(stream);
    let mut start = String::new();
    if reader.read_line(&mut start).is_err() || start.is_empty() {
        return;
    }
    let mut parts = start.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let mut len = 0usize;
    let mut content_type = String::new();
    let mut x_test = String::new();
    let mut signature = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        if line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = lower.strip_prefix("content-type:") {
            content_type = v.trim().to_string();
        } else if let Some((_, v)) = line.split_once(':') {
            if lower.starts_with("x-test:") {
                x_test = v.trim().to_string();
            } else if lower.starts_with("x-signature:") {
                signature = v.trim().to_string();
            }
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok();
    let body = String::from_utf8_lossy(&body).to_string();
    *seen.lock().unwrap() = Some(Received {
        method,
        path,
        content_type,
        x_test,
        signature,
        body,
    });
    // A distinctive response the workflow can observe end-to-end.
    let resp_body = br#"{"pong":true,"n":42,"nested":{"ok":"yes"}}"#;
    let mut s = reader.into_inner();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp_body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(resp_body);
    let _ = s.flush();
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
    fn events(&self, name: &str) -> Vec<Value> {
        self.stderr()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("http-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn http daemon");
    Daemon { child, stderr_path }
}

fn wait_for<F: Fn() -> bool>(f: F, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn an_http_node_posts_json_and_the_structured_response_flows_into_the_run() {
    let (port, seen, _mock) = spawn_mock_rest();
    let cfg_path = common::unique_path("agentd-http", "yaml");
    let cfg = format!(
        "config_version: \"2\"\n\
         agent:\n  name: caller\n  instruction: make a call\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1\n  model: mock\n\
         store:\n  kind: memory\n\
         workflows:\n  - name: call\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         \x20     call: {{kind: http, depends_on: [s], method: POST, url: \"http://127.0.0.1:{port}/echo\", json: {{hello: world, n: 7}}, headers: {{X-Test: abc}}, allow_private: true}}\n\
         \x20     done: {{kind: finish, depends_on: [call], output: \"{{{{steps.call.output}}}}\"}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n  log_content: true\n"
    );
    std::fs::write(&cfg_path, &cfg).unwrap();
    let daemon = spawn_daemon(&cfg_path);

    // The `once` start fires the workflow, which POSTs to the mock and finishes.
    assert!(
        wait_for(
            || daemon
                .events("run.done")
                .iter()
                .any(|e| e["status"] == "completed"),
            15
        ),
        "the http workflow ran to completion:\n{}",
        daemon.stderr()
    );

    // 1. The outbound request arrived with the right method, path, headers, body.
    let got = seen
        .lock()
        .unwrap()
        .clone()
        .expect("mock received a request");
    assert_eq!(got.method, "POST", "the http node used the declared method");
    assert_eq!(got.path, "/echo", "…at the declared path");
    assert!(
        got.content_type.contains("application/json"),
        "a json body set Content-Type: {:?}",
        got.content_type
    );
    assert_eq!(got.x_test, "abc", "a declared header was sent");
    let sent: Value = serde_json::from_str(&got.body).expect("the body is the templated json");
    assert_eq!(sent["hello"], "world");
    assert_eq!(sent["n"], 7);

    // 2. The structured response flowed into the finish output (log_content on).
    let done = daemon.events("run.done");
    let out = &done
        .iter()
        .find(|e| e["status"] == "completed")
        .expect("a completed run.done")["output"];
    assert_eq!(out["status"], 200, "the http status is observable: {out}");
    assert_eq!(out["ok"], true, "a 2xx is ok");
    assert_eq!(out["json"]["n"], 42, "the parsed json body is observable");
    assert_eq!(
        out["json"]["nested"]["ok"], "yes",
        "…including nested fields"
    );

    std::fs::remove_file(&cfg_path).ok();
}

#[test]
fn an_http_node_emits_a_signed_webhook_the_receiver_can_verify() {
    let (port, seen, _mock) = spawn_mock_rest();
    let secret = "emit-secret";
    let cfg_path = common::unique_path("agentd-http-sign", "yaml");
    // `sign` HMAC-signs the exact request body → `X-Signature: sha256=<hex>`,
    // so the node is a verifiable webhook emitter. The secret comes through a
    // `{{secret:…}}` reference (never inline), fed by an env var.
    let cfg = format!(
        "config_version: \"2\"\n\
         agent:\n  name: emitter\n  instruction: emit\n  preflight: never\n\
         intelligence:\n  endpoints: http://127.0.0.1:1\n  model: mock\n\
         store:\n  kind: memory\n\
         workflows:\n  - name: emit\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         \x20     call: {{kind: http, depends_on: [s], method: POST, url: \"http://127.0.0.1:{port}/deliver\", json: {{event: deploy, id: 99}}, sign: {{secret: \"{{{{secret:HOOK_SECRET}}}}\"}}, allow_private: true}}\n\
         \x20     done: {{kind: finish, depends_on: [call]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    );
    std::fs::write(&cfg_path, &cfg).unwrap();
    let stderr_path = common::unique_path("http-sign-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .env("HOOK_SECRET", secret)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn emitter daemon");

    let got = wait_for(|| seen.lock().unwrap().is_some(), 15);
    let logs = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert!(
        got,
        "the signed http node delivered to the receiver:\n{logs}"
    );
    let got = seen.lock().unwrap().clone().unwrap();

    // The receiver verifies the signature exactly as our inbound `webhook` node
    // would: recompute HMAC-SHA256 over the body with the shared secret.
    let mac = agentd::sha::hmac_sha256(secret.as_bytes(), got.body.as_bytes());
    let expect = format!("sha256={}", agentd::sha::to_hex(&mac));
    assert_eq!(
        got.signature, expect,
        "the emitted X-Signature verifies against the delivered body"
    );

    let _ = child.kill();
    let _ = child.wait();
    std::fs::remove_file(&cfg_path).ok();
    std::fs::remove_file(&stderr_path).ok();
}

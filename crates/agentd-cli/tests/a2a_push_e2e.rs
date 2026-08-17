// SPDX-License-Identifier: AGPL-3.0-only
//! **Push notifications end to end**: a caller registers a webhook, and agentd
//! POSTs the task's updates to it.
//!
//! The interesting assertions are not that a delivery arrives — they are the
//! refusals. The URL comes from a peer, so the feature is off unless an operator
//! turned it on, and a target that points somewhere agentd should not reach is
//! rejected while the caller is still there to be told why.
#![cfg(feature = "a2a")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod common;

use std::process::{Child, Command, Stdio};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn post_raw(addr: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect a2a");
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    loop {
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
    }
    let mut b = String::new();
    r.read_to_string(&mut b).unwrap();
    b
}

fn rpc(addr: &str, id: i64, method: &str, params: Value) -> Value {
    let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let raw = post_raw(addr, &body);
    serde_json::from_str(&raw).unwrap_or_else(|_| panic!("non-JSON response: {raw:?}"))
}

/// One delivery, as the receiver saw it: the headers, then the body.
type Delivery = (Vec<(String, String)>, Value);

/// A webhook receiver: records every delivery, headers included.
struct Hook {
    url: String,
    seen: Arc<Mutex<Vec<Delivery>>>,
}

fn spawn_hook() -> Hook {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Delivery>>> = Arc::default();
    let out = Arc::clone(&seen);
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let out = Arc::clone(&out);
            std::thread::spawn(move || {
                conn.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut w = conn.try_clone().unwrap();
                let mut r = BufReader::new(conn);
                let mut start = String::new();
                if r.read_line(&mut start).unwrap_or(0) == 0 {
                    return;
                }
                let mut headers = Vec::new();
                let mut len = 0usize;
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).unwrap_or(0) == 0 {
                        return;
                    }
                    if l.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = l.split_once(':') {
                        let k = k.trim().to_ascii_lowercase();
                        if k == "content-length" {
                            len = v.trim().parse().unwrap_or(0);
                        }
                        headers.push((k, v.trim().to_string()));
                    }
                }
                let mut body = vec![0u8; len];
                r.read_exact(&mut body).ok();
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                out.lock().unwrap().push((headers, v));
                let _ = w.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
            });
        }
    });
    Hook { url, seen }
}

fn config(llm: &str, port: u16, push: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: push-e2e\n  instruction: You are a helpful test agent.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         a2a:\n  listen: http://127.0.0.1:{port}\n{push}\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

struct MockLlm {
    child: Child,
    addr_file: String,
    uri: String,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.addr_file);
    }
}
fn spawn_mock_llm(playbook: &Value) -> MockLlm {
    let pb = common::unique_path("push-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("push-mock-llm", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    MockLlm {
        child,
        addr_file,
        uri: format!("http://{addr}"),
    }
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
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

/// Spawn on a probed free port and return the authority it actually bound; the
/// probe→bind gap is a real race under parallel CI, so a lost bind is retried.
fn boot(cfg_for: impl Fn(u16) -> String) -> (Daemon, String, String) {
    for _ in 0..5 {
        let path = common::unique_path("agentd-push", "yaml");
        std::fs::write(&path, cfg_for(free_port())).unwrap();
        let stderr_path = common::unique_path("push-daemon", "log");
        let errf = std::fs::File::create(&stderr_path).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(errf))
            .spawn()
            .expect("spawn agentd");
        let daemon = Daemon { child, stderr_path };
        if let Some(addr) = common::try_a2a_bound(&daemon.stderr_path, Duration::from_secs(15)) {
            return (daemon, addr, path);
        }
        std::fs::remove_file(&path).ok();
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

fn wait_for<T>(mut f: impl FnMut() -> Option<T>, secs: u64, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_registered_webhook_receives_the_task_and_the_callers_token() {
    // A deliberately slow turn, so the webhook is registered while the task is
    // still working and the transition to `completed` is a real delivery rather
    // than a race with one.
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "done at last", "delay_ms": 2500}]}));
    let hook = spawn_hook();
    // `allow_private` because the receiver in this test is loopback, which is
    // exactly the decision the flag exists to make explicit.
    let (_daemon, addr, cfg_path) = boot(|p| {
        config(
            &llm.uri,
            p,
            "  push:\n    enabled: true\n    allow_private: true\n",
        )
    });

    // A natural-language send that returns as soon as the task exists, so the
    // work is still in flight when the webhook is attached.
    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "role": "ROLE_USER",
                           "parts": [{"text": "take your time"}]},
               "configuration": {"returnImmediately": true}}),
    );
    let task_id = sent["result"]["task"]["id"]
        .as_str()
        .unwrap_or_else(|| panic!("a task: {sent}"))
        .to_string();

    let registered = rpc(
        &addr,
        2,
        "CreateTaskPushNotificationConfig",
        // The spec's request *is* the config: a flat `TaskPushNotificationConfig`.
        json!({"taskId": task_id, "url": hook.url, "token": "caller-token"}),
    );
    assert!(
        registered.get("error").is_none(),
        "registration should succeed: {registered}"
    );
    let config_id = registered["result"]["id"]
        .as_str()
        .expect("a config id")
        .to_string();

    // The turn finishes on its own; that transition is what gets delivered.
    let (headers, body) = wait_for(
        || hook.seen.lock().unwrap().first().cloned(),
        20,
        "a delivery",
    );
    // The body is the task, in the same shape a streaming caller would see.
    assert_eq!(body["id"], task_id.as_str(), "{body}");
    assert!(body["status"]["state"].as_str().is_some(), "{body}");
    // The caller's token comes back, so the receiver can tell a real delivery
    // from a stray POST at a URL somebody guessed.
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-a2a-notification-token" && v == "caller-token"),
        "{headers:?}"
    );

    // Read-back and delete round out the family.
    let listed = rpc(
        &addr,
        4,
        "ListTaskPushNotificationConfigs",
        json!({"taskId": task_id}),
    );
    assert!(
        listed["result"]["configs"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{listed}"
    );
    let deleted = rpc(
        &addr,
        5,
        "DeleteTaskPushNotificationConfig",
        json!({"taskId": task_id, "pushNotificationConfigId": config_id}),
    );
    assert!(deleted.get("error").is_none(), "{deleted}");

    std::fs::remove_file(&cfg_path).ok();
}

#[test]
fn a_target_agentd_should_not_reach_is_refused_at_registration() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    // Enabled, but WITHOUT allow_private: the ordinary production posture.
    let (_daemon, addr, cfg_path) = boot(|p| config(&llm.uri, p, "  push:\n    enabled: true\n"));

    let sent = rpc(
        &addr,
        1,
        "SendMessage",
        json!({"message": {"messageId": "m1", "role": "ROLE_USER",
               "parts": [{"data": {"agentd": {"op": "status"}}}]}}),
    );
    let task_id = sent["result"]["task"]["id"].as_str().unwrap().to_string();

    // The cloud metadata endpoint: the canonical thing a peer would like agentd
    // to fetch on its behalf.
    let refused = rpc(
        &addr,
        2,
        "CreateTaskPushNotificationConfig",
        json!({"taskId": task_id, "url": "http://169.254.169.254/latest/meta-data/"}),
    );
    assert_eq!(
        refused["error"]["code"], -32602,
        "a link-local target must be refused with a reason: {refused}"
    );

    std::fs::remove_file(&cfg_path).ok();
}

#[test]
fn push_is_off_unless_an_operator_turns_it_on() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (_daemon, addr, cfg_path) = boot(|p| config(&llm.uri, p, ""));

    // The card is a promise: with the feature off it must not claim the
    // capability.
    let card = rpc(&addr, 1, "GetAgentCard", json!({}));
    assert_eq!(
        card["result"]["capabilities"]["pushNotifications"], false,
        "{card}"
    );

    let sent = rpc(
        &addr,
        2,
        "SendMessage",
        json!({"message": {"messageId": "m1", "role": "ROLE_USER",
               "parts": [{"data": {"agentd": {"op": "status"}}}]}}),
    );
    let task_id = sent["result"]["task"]["id"].as_str().unwrap().to_string();

    // …and asking anyway is a clean refusal, not a silent no-op.
    let refused = rpc(
        &addr,
        3,
        "CreateTaskPushNotificationConfig",
        json!({"taskId": task_id, "url": "https://hooks.example/x"}),
    );
    assert!(
        refused["error"]["code"].as_i64().is_some(),
        "a disclaimed capability must refuse: {refused}"
    );

    std::fs::remove_file(&cfg_path).ok();
}

#[test]
fn the_extended_card_is_the_authenticated_one() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "unused"}]}));
    let (_daemon, addr, cfg_path) = boot(|p| config(&llm.uri, p, ""));

    // Loopback with nothing configured resolves to the operator, so this call
    // is authenticated and the extended card is served.
    let extended = rpc(&addr, 1, "GetExtendedAgentCard", json!({}));
    assert!(
        extended.get("error").is_none(),
        "an authenticated caller gets the extended card: {extended}"
    );
    assert_eq!(extended["result"]["name"], "agentd");
    assert_eq!(
        extended["result"]["supportsAuthenticatedExtendedCard"], true,
        "{extended}"
    );

    std::fs::remove_file(&cfg_path).ok();
}

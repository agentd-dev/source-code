// SPDX-License-Identifier: AGPL-3.0-only
//! agentd 2.0 **inbound webhook** surface (RFC 0027) end to end: a daemon binds a
//! dedicated webhook listener; a signed HTTP POST fires a workflow run, an
//! unsigned/badly-signed request is rejected 401, and a replay of the same
//! idempotency key is deduplicated (no second run).
#![cfg(all(unix, feature = "a2a"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// POST `body` to `path` with extra headers; returns `(status_code, body)`.
fn post(addr: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).expect("connect webhook");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
    }
    let mut b = String::new();
    reader.read_to_string(&mut b).unwrap();
    (code, b)
}

/// Like [`post`], but also returns the raw response header block.
fn post_hdrs(
    addr: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).expect("connect webhook");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s.flush().unwrap();
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status).unwrap();
    let code: u16 = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut hdrs = String::new();
    loop {
        let mut l = String::new();
        reader.read_line(&mut l).unwrap();
        if l.trim().is_empty() {
            break;
        }
        hdrs.push_str(&l);
    }
    let mut b = String::new();
    reader.read_to_string(&mut b).unwrap();
    (code, hdrs, b)
}

fn sign(secret: &str, body: &str) -> String {
    let mac = agentd::sha::hmac_sha256(secret.as_bytes(), body.as_bytes());
    format!("sha256={}", agentd::sha::to_hex(&mac))
}

fn wait_ready(addr: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "webhook listener never came up");
        std::thread::sleep(Duration::from_millis(25));
    }
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
    let pb = common::unique_path("wh-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("wh-mock-llm", "addr");
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
fn spawn_daemon(config: &str, env: &[(&str, &str)]) -> Daemon {
    let stderr_path = common::unique_path("wh-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", config]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn webhook daemon");
    Daemon { child, stderr_path }
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-webhook", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

fn config(llm: &str, port: u16) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: hook\n  instruction: You handle webhooks.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         workflows:\n  - name: on-hook\n    steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/deploy, methods: [POST], auth: {{hmac: {{secret: \"{{{{secret:HOOK_SECRET}}}}\"}}}}}}\n\
         \x20     a: {{kind: agent, depends_on: [h], instruction: \"handle it\"}}\n\
         \x20     f: {{kind: finish, depends_on: [a]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
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
fn a_signed_webhook_fires_the_workflow_bad_signature_is_rejected_and_replays_dedupe() {
    let secret = "topsecret";
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "handled the webhook"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&config(&llm.uri, port));
    let daemon = spawn_daemon(&cfg, &[("HOOK_SECRET", secret)]);
    wait_ready(&addr);

    let body = r#"{"ref":"refs/heads/main","action":"deploy"}"#;

    // 1. A correctly-signed POST fires the workflow run.
    let sig = sign(secret, body);
    let (code, _) = post(
        &addr,
        "/hooks/deploy",
        &[("X-Signature", &sig), ("Idempotency-Key", "evt-1")],
        body,
    );
    assert_eq!(code, 202, "a signed webhook is accepted");
    assert!(
        wait_for(
            || daemon
                .events("run.done")
                .iter()
                .any(|e| e["status"] == "completed"),
            10
        ),
        "the webhook fired a workflow run to completion:\n{}",
        daemon.stderr()
    );
    let fired = daemon.events("start.fired");
    assert!(
        fired.iter().any(|e| e["kind"] == "webhook"),
        "a webhook start node fired: {fired:?}"
    );
    let runs_after_first = daemon.events("run.start").len();

    // 2. A bad signature is rejected 401 and fires nothing.
    let (code, _) = post(
        &addr,
        "/hooks/deploy",
        &[
            ("X-Signature", "sha256=deadbeef"),
            ("Idempotency-Key", "evt-2"),
        ],
        body,
    );
    assert_eq!(code, 401, "a bad signature is rejected");

    // 3. A replay of the same idempotency key is deduplicated (no new run).
    let (code, dup_body) = post(
        &addr,
        "/hooks/deploy",
        &[("X-Signature", &sig), ("Idempotency-Key", "evt-1")],
        body,
    );
    assert_eq!(code, 200, "a replay returns 200");
    assert!(
        dup_body.contains("duplicate"),
        "the replay is marked a duplicate: {dup_body}"
    );
    // Give the loop a moment; assert no additional run started.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        daemon.events("run.start").len(),
        runs_after_first,
        "the deduplicated replay did not fire another run:\n{}",
        daemon.stderr()
    );

    std::fs::remove_file(&cfg).ok();
}

fn await_config(llm: &str, port: u16) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: hookawait\n  instruction: You process callbacks.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         workflows:\n  - name: await-cb\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         \x20     w:    {{kind: wait, on: webhook, webhook: {{path: /hooks/cb/test}}, depends_on: [s], timeout: 30s}}\n\
         \x20     act:  {{kind: agent, depends_on: [w], instruction: \"process it\"}}\n\
         \x20     done: {{kind: finish, depends_on: [act]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

#[test]
fn a_webhook_await_pauses_a_workflow_until_the_callback_arrives() {
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "processed the callback"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&await_config(&llm.uri, port));
    let daemon = spawn_daemon(&cfg, &[]);
    wait_ready(&addr);

    // The `once` run reaches the `wait: {on: webhook}` and suspends.
    assert!(
        wait_for(
            || daemon
                .events("webhook.await.armed")
                .iter()
                .any(|e| e["path"] == "/hooks/cb/test"),
            10
        ),
        "the workflow armed a webhook await:\n{}",
        daemon.stderr()
    );
    assert!(
        daemon.events("run.done").is_empty(),
        "the run is still waiting for the callback:\n{}",
        daemon.stderr()
    );

    // POST the callback → the wait resumes → the workflow finishes.
    let (code, body) = post(&addr, "/hooks/cb/test", &[], r#"{"ok":true}"#);
    assert_eq!(code, 200, "the callback is accepted: {body}");
    assert!(
        body.contains("resumed"),
        "the callback resumed a waiter: {body}"
    );
    assert!(
        wait_for(
            || daemon
                .events("run.done")
                .iter()
                .any(|e| e["status"] == "completed"),
            10
        ),
        "the workflow completed after the callback:\n{}",
        daemon.stderr()
    );
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_respond_sync_webhook_returns_the_run_result_inline() {
    let secret = "s3cr3t";
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "processed synchronously"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&format!(
        "config_version: \"2\"\n\
         agent:\n  name: sync\n  instruction: You process.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         workflows:\n  - name: sync-hook\n    steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/sync, methods: [POST], respond: sync, auth: {{hmac: {{secret: \"{{{{secret:HOOK_SECRET}}}}\"}}}}}}\n\
         \x20     a: {{kind: agent, depends_on: [h], instruction: \"process it\"}}\n\
         \x20     f: {{kind: finish, depends_on: [a], output: \"done-sync\"}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n",
        llm.uri
    ));
    let _daemon = spawn_daemon(&cfg, &[("HOOK_SECRET", secret)]);
    wait_ready(&addr);
    let body = r#"{"x":1}"#;
    let sig = sign(secret, body);
    // A respond:sync webhook holds the HTTP response until the run finishes.
    let (code, resp) = post(&addr, "/hooks/sync", &[("X-Signature", &sig)], body);
    assert_eq!(
        code, 200,
        "respond:sync returns the run result inline (not 202): {resp}"
    );
    assert!(
        resp.contains("completed"),
        "the sync response carries the terminal status: {resp}"
    );
    assert!(
        resp.contains("done-sync"),
        "the sync response carries the run output: {resp}"
    );
    std::fs::remove_file(&cfg).ok();
}

// ---- arrival-rate limiting + pressure shedding ------------------------------

fn rate_config(llm: &str, port: u16) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: ratehook\n  instruction: You handle webhooks.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: memory\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         workflows:\n  - name: on-hook\n    steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/rated, methods: [POST], rate: \"2/60s\", auth: {{hmac: {{secret: \"{{{{secret:HOOK_SECRET}}}}\"}}}}}}\n\
         \x20     a: {{kind: agent, depends_on: [h], instruction: \"handle it\"}}\n\
         \x20     f: {{kind: finish, depends_on: [a]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

#[test]
fn a_rated_route_admits_its_burst_then_answers_429_with_retry_after() {
    let secret = "topsecret";
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "handled"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = write_config(&rate_config(&llm.uri, port));
    let daemon = spawn_daemon(&cfg, &[("HOOK_SECRET", secret)]);
    wait_ready(&addr);

    let body = r#"{"n":1}"#;
    let sig = sign(secret, body);
    // The burst (2 tokens over 60s: no refill inside this test) is admitted…
    for n in 1..=2 {
        let (code, _) = post(
            &addr,
            "/hooks/rated",
            &[
                ("X-Signature", &sig),
                ("Idempotency-Key", &format!("r-{n}")),
            ],
            body,
        );
        assert_eq!(code, 202, "request {n} of the burst is admitted");
    }
    // …the third is rate-limited, with a Retry-After a client can pace off.
    let (code, hdrs, resp) = post_hdrs(
        &addr,
        "/hooks/rated",
        &[("X-Signature", &sig), ("Idempotency-Key", "r-3")],
        body,
    );
    assert_eq!(code, 429, "the post-burst request is refused: {resp}");
    assert!(
        hdrs.to_ascii_lowercase().contains("retry-after: 30"),
        "429 carries Retry-After (60s window / burst 2): {hdrs}"
    );
    assert!(resp.contains("rate limited"), "the body says why: {resp}");
    // The two admitted requests really did fire runs (auth+rate compose).
    assert!(
        wait_for(|| daemon.events("run.start").len() >= 2, 10),
        "both admitted webhooks fired runs:\n{}",
        daemon.stderr()
    );
    std::fs::remove_file(&cfg).ok();
}

fn shed_config(llm: &str, port: u16, store_dir: &str) -> String {
    format!(
        "config_version: \"2\"\n\
         agent:\n  name: shedhook\n  instruction: You handle webhooks.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: file\n  file:\n    path: {store_dir}\n    min_free: 999999GB\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         workflows:\n  - name: on-hook\n    steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/shed, methods: [POST], auth: {{hmac: {{secret: \"{{{{secret:HOOK_SECRET}}}}\"}}}}}}\n\
         \x20     a: {{kind: agent, depends_on: [h], instruction: \"handle it\"}}\n\
         \x20     f: {{kind: finish, depends_on: [a]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n"
    )
}

#[test]
fn a_daemon_under_disk_pressure_sheds_webhooks_with_429() {
    let secret = "topsecret";
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "never reached"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // min_free of ~1PB puts any real filesystem below the shed threshold: the
    // daemon comes up already shedding, so the gate itself is what we observe.
    let store_dir = common::unique_path("wh-shed-store", "d");
    std::fs::create_dir_all(&store_dir).unwrap();
    let cfg = write_config(&shed_config(&llm.uri, port, &store_dir));
    let daemon = spawn_daemon(&cfg, &[("HOOK_SECRET", secret)]);
    wait_ready(&addr);

    let body = r#"{"n":1}"#;
    let sig = sign(secret, body);
    let (code, hdrs, resp) = post_hdrs(
        &addr,
        "/hooks/shed",
        &[("X-Signature", &sig), ("Idempotency-Key", "s-1")],
        body,
    );
    assert_eq!(code, 429, "a shedding daemon refuses admission: {resp}");
    assert!(
        hdrs.to_ascii_lowercase().contains("retry-after"),
        "the shed refusal carries Retry-After: {hdrs}"
    );
    assert!(resp.contains("shedding"), "the body says why: {resp}");
    // …and the operator was told, once, at the transition.
    assert!(
        wait_for(|| !daemon.events("pressure.shed").is_empty(), 5),
        "the shed transition is logged:\n{}",
        daemon.stderr()
    );
    // A bad signature still answers 401, not 429: auth is checked first, so an
    // unauthenticated probe learns nothing about our load.
    let (code, _) = post(
        &addr,
        "/hooks/shed",
        &[
            ("X-Signature", "sha256=deadbeef"),
            ("Idempotency-Key", "s-2"),
        ],
        body,
    );
    assert_eq!(code, 401, "auth precedes admission");
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&store_dir).ok();
}

/// Free bytes on `/` (statvfs), so the test can engineer the WARN band.
fn free_bytes_root() -> u64 {
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        let path = std::ffi::CString::new("/").unwrap();
        assert_eq!(libc::statvfs(path.as_ptr(), &mut st), 0, "statvfs /");
        (st.f_bavail as u64) * (st.f_frsize as u64)
    }
}

#[test]
fn at_warn_a_low_priority_route_sheds_while_normal_still_admits() {
    let secret = "topsecret";
    let llm = spawn_mock_llm(&json!({"turns": [{"content": "handled"}]}));
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    // min_free at 2/3 of the actual free space puts the daemon in the WARN
    // band (shed < free < 2×shed): low-priority admissions shed, normal ones
    // do not — priority's teeth, observed on the wire.
    let min_free = free_bytes_root() * 2 / 3;
    let store_dir = common::unique_path("wh-warn-store", "d");
    std::fs::create_dir_all(&store_dir).unwrap();
    let cfg = write_config(&format!(
        "config_version: \"2\"\n\
         agent:\n  name: warnhook\n  instruction: You handle webhooks.\n  preflight: never\n\
         intelligence:\n  endpoints: {llm}\n  model: mock\n\
         store:\n  kind: file\n  file:\n    path: {store_dir}\n    min_free: \"{min_free}\"\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         workflows:\n  - name: bulk\n    priority: low\n    steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/bulk, methods: [POST], auth: {{hmac: {{secret: \"{{{{secret:HOOK_SECRET}}}}\"}}}}}}\n\
         \x20     a: {{kind: agent, depends_on: [h], instruction: \"handle it\"}}\n\
         \x20     f: {{kind: finish, depends_on: [a]}}\n\
         \x20 - name: urgent\n    steps:\n\
         \x20     h: {{kind: webhook, path: /hooks/urgent, methods: [POST], auth: {{hmac: {{secret: \"{{{{secret:HOOK_SECRET}}}}\"}}}}}}\n\
         \x20     a: {{kind: agent, depends_on: [h], instruction: \"handle it\"}}\n\
         \x20     f: {{kind: finish, depends_on: [a]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n",
        llm = llm.uri
    ));
    let daemon = spawn_daemon(&cfg, &[("HOOK_SECRET", secret)]);
    wait_ready(&addr);

    let body = r#"{"n":1}"#;
    let sig = sign(secret, body);
    let (code, _, resp) = post_hdrs(
        &addr,
        "/hooks/bulk",
        &[("X-Signature", &sig), ("Idempotency-Key", "w-1")],
        body,
    );
    assert_eq!(code, 429, "low priority sheds at warn: {resp}");
    assert!(resp.contains("low-priority"), "the body says why: {resp}");
    let (code, _) = post(
        &addr,
        "/hooks/urgent",
        &[("X-Signature", &sig), ("Idempotency-Key", "w-2")],
        body,
    );
    assert_eq!(code, 202, "normal priority still admits at warn");
    assert!(
        wait_for(|| !daemon.events("pressure.warn").is_empty(), 5),
        "the warn transition is logged:\n{}",
        daemon.stderr()
    );
    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&store_dir).ok();
}

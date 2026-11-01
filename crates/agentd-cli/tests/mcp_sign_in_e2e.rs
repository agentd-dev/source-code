// SPDX-License-Identifier: AGPL-3.0-only
//! **Sign in on use** (RFC 0055 §4.3), end to end. A `connect: on_use` MCP
//! server answers only a bearer token from a mock authorization server's
//! device grant, and the login cache holds none. A workflow step that calls
//! it waits: agentd starts the device grant, says where to sign in
//! (`mcp.sign_in.required`) and shows it as a gate on the run's A2A task
//! (`input-required`, the URL and the code in its question, `mcpSignIn` in
//! its metadata). The person approves at the authorization server; the token
//! is cached, the server is dialed with it, and the step completes. A gate
//! nobody answers within the step's `timeout` fails the step
//! `sign_in_timeout`; a denied grant fails it `sign_in_denied`; a restart in
//! the middle of the wait closes the old gate, starts a new sign-in, and the
//! step completes once the person signs in.
#![cfg(all(unix, feature = "a2a", feature = "oauth"))]

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use common::{SendMessage, free_port, rpc_result as rpc};

const TOKEN: &str = "signed-in-token";
const USER_CODE: &str = "WDJB-MJHT";

/// What the mock authorization server answers a device-token poll with.
const PENDING: u8 = 0;
const APPROVED: u8 = 1;
const DENIED: u8 = 2;

/// A mock authorization server: RFC 8414 metadata, the device-authorization
/// endpoint, and a token endpoint that answers `authorization_pending` until
/// the test approves (or denies).
struct Idp {
    issuer: String,
    answer: Arc<AtomicU8>,
    devices: Arc<AtomicUsize>,
}

impl Idp {
    fn spawn() -> Idp {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let issuer = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let answer = Arc::new(AtomicU8::new(PENDING));
        let devices = Arc::new(AtomicUsize::new(0));
        let (iss, ans, dev) = (issuer.clone(), Arc::clone(&answer), Arc::clone(&devices));
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let (iss, ans, dev) = (iss.clone(), Arc::clone(&ans), Arc::clone(&dev));
                std::thread::spawn(move || serve_idp(c, &iss, &ans, &dev));
            }
        });
        Idp {
            issuer,
            answer,
            devices,
        }
    }

    fn set(&self, a: u8) {
        self.answer.store(a, Ordering::SeqCst);
    }
}

fn serve_idp(mut s: TcpStream, issuer: &str, answer: &AtomicU8, devices: &AtomicUsize) {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    let _ = r.read_exact(&mut body);
    let (status, reply) = if path.starts_with("/.well-known/") {
        (
            200,
            json!({"issuer": issuer, "token_endpoint": format!("{issuer}/token"),
                   "device_authorization_endpoint": format!("{issuer}/device")}),
        )
    } else if path == "/device" {
        devices.fetch_add(1, Ordering::SeqCst);
        (
            200,
            json!({"device_code": "dev-code", "user_code": USER_CODE,
                   "verification_uri": format!("{issuer}/activate"),
                   "interval": 1, "expires_in": 600}),
        )
    } else if path == "/token" {
        match answer.load(Ordering::SeqCst) {
            APPROVED => (
                200,
                json!({"access_token": TOKEN, "token_type": "Bearer", "expires_in": 3600,
                       "refresh_token": "refresh"}),
            ),
            DENIED => (400, json!({"error": "access_denied"})),
            _ => (400, json!({"error": "authorization_pending"})),
        }
    } else {
        (404, json!({}))
    };
    let body = reply.to_string();
    let _ = write!(
        s,
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The MCP server: the built-in mock, answering only [`TOKEN`].
fn spawn_mcp() -> String {
    let mut opts = agentd::mcp::mock_http::Options::new("mock://resource");
    opts.bearer = Some(TOKEN.to_string());
    format!("{}/mcp", common::mock_in_thread(opts))
}

struct MockLlm {
    child: Child,
}
impl Drop for MockLlm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn spawn_mock_llm() -> (MockLlm, String) {
    let pb = common::unique_path("signin-playbook", "json");
    std::fs::write(&pb, json!({"turns": [{"content": "unused"}]}).to_string()).unwrap();
    let addr_file = common::unique_path("signin-mock-llm", "addr");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &addr_file, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mock llm");
    let addr = common::read_addr_file(&addr_file);
    (MockLlm { child }, format!("http://{addr}"))
}

struct Daemon {
    child: Child,
    log: String,
}
impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn wait_log(&self, needle: &str, n: usize, secs: u64) -> String {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let log = self.log();
            if log.matches(needle).count() >= n {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "timeout waiting for {needle} x{n}; log:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(80));
        }
    }
    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Everything one case runs against.
struct Case {
    _llm: MockLlm,
    llm: String,
    idp: Idp,
    mcp: String,
    dir: tempfile::TempDir,
}

impl Case {
    fn new() -> Case {
        let (llm_child, llm) = spawn_mock_llm();
        Case {
            _llm: llm_child,
            llm,
            idp: Idp::spawn(),
            mcp: spawn_mcp(),
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// The daemon on a free port, its state and login cache under the
    /// case's directory, the step's `timeout` as `step_timeout`.
    fn start(&self, step_timeout: &str) -> (Daemon, String) {
        let dir = self.dir.path().display().to_string();
        std::fs::create_dir_all(format!("{dir}/creds")).unwrap();
        for _ in 0..5 {
            let port = free_port();
            let cfg = format!(
                "agent:\n  name: signin-e2e\n  instruction: test agent\n  preflight: never\n\
                 intelligence:\n  endpoints: {llm}\n  model: mock\n\
                 store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
                 a2a:\n  listen: http://127.0.0.1:{port}\n  introspection:\n    enabled: true\n\
                 observability:\n  log_level: info\n\
                 mcp:\n  servers:\n    - name: vault\n      endpoint: {mcp}\n      connect: on_use\n      \
                 auth: {{kind: oauth2, grant: device, issuer: \"{issuer}\", client_id: agentd-test}}\n\
                 workflows:\n  - name: fetch\n    steps:\n      s: {{kind: manual}}\n      \
                 call: {{kind: mcp.tool, server: vault, tool: state.put, args: {{key: k, value: v}}, depends_on: [s]{step_timeout}}}\n      \
                 f: {{kind: finish, depends_on: [call], output: done}}\n",
                llm = self.llm,
                mcp = self.mcp,
                issuer = self.idp.issuer,
            );
            let path = format!("{dir}/agentd-{port}.yaml");
            std::fs::write(&path, cfg).unwrap();
            let log = format!("{dir}/agentd-{port}.log");
            let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
                .args(["--config", &path])
                .env("AGENTD_CRED_DIR", format!("{dir}/creds"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
                .spawn()
                .expect("spawn agentd");
            let d = Daemon { child, log };
            if let Some(addr) = common::try_a2a_bound(&d.log, Duration::from_secs(15)) {
                return (d, addr);
            }
        }
        panic!("the daemon never bound an A2A listener (5 attempts)");
    }
}

fn wait_task(addr: &str, id: &str, secs: u64, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let t = rpc(addr, 900, "GetTask", json!({"id": id}));
        if pred(&t) {
            return t;
        }
        assert!(Instant::now() < deadline, "timeout: {what}; last: {t}");
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn state(t: &Value) -> &str {
    t["status"]["state"].as_str().unwrap_or("")
}

fn text(t: &Value) -> String {
    t["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Start `fetch`: its task's id.
fn run_fetch(addr: &str) -> String {
    let r = SendMessage::command("workflow.run", json!({"workflow": "fetch"}))
        .return_immediately()
        .result(addr);
    r["task"]["id"].as_str().expect("a task").to_string()
}

/// The step `call` of the only run, as `run.get` reads it.
fn call_step(addr: &str) -> Value {
    let read = |op: &str, args: Value| {
        SendMessage::command(op, args).result(addr)["message"]["parts"][0]["data"].clone()
    };
    let ws = read("workflow.status", json!({}));
    let run = ws["runs"][0]["run"].as_str().expect("a run").to_string();
    read("run.get", json!({"run": run}))["run"]["steps"]["call"].clone()
}

/// The gate shows where to sign in; the person approves at the
/// authorization server; the step completes with the server's answer.
#[test]
fn a_step_signs_in_at_a_gate_and_completes() {
    let case = Case::new();
    let (d, addr) = case.start("");
    let task = run_fetch(&addr);
    let log = d.wait_log("\"mcp.sign_in.required\"", 1, 20);
    assert!(
        log.contains(USER_CODE) && log.contains("\"method\":\"device\""),
        "{log}"
    );
    let gated = wait_task(&addr, &task, 10, "the sign-in gate", |t| {
        state(t) == "TASK_STATE_INPUT_REQUIRED"
    });
    let q = text(&gated);
    assert!(
        q.contains(USER_CODE) && q.contains(&format!("{}/activate", case.idp.issuer)),
        "{gated}"
    );
    // Anything but cancel is refused, the gate left open.
    let refused = SendMessage::text("here is my password")
        .task(&task)
        .return_immediately()
        .post(&addr);
    assert!(refused["error"].to_string().contains("cancel"), "{refused}");
    assert_eq!(
        state(&rpc(&addr, 902, "GetTask", json!({"id": task}))),
        "TASK_STATE_INPUT_REQUIRED"
    );

    case.idp.set(APPROVED);
    let done = wait_task(&addr, &task, 30, "the run's end", |t| {
        state(t) == "TASK_STATE_COMPLETED"
    });
    let log = d.log();
    assert!(log.contains("\"mcp.sign_in.done\""), "{log}");
    assert_eq!(call_step(&addr)["status"], "done", "{done}");
    // The token is in the login cache, under the server's target.
    let cached = std::fs::read_dir(case.dir.path().join("creds"))
        .unwrap()
        .count();
    assert!(cached >= 1, "nothing cached");
}

/// A gate nobody answers within the step's timeout fails the step
/// `sign_in_timeout`.
#[test]
fn an_unanswered_gate_fails_the_step_at_its_timeout() {
    let case = Case::new();
    let (d, addr) = case.start(", timeout: 3s");
    let task = run_fetch(&addr);
    d.wait_log("\"mcp.sign_in.required\"", 1, 20);
    let ended = wait_task(&addr, &task, 30, "the run's failure", |t| {
        state(t) == "TASK_STATE_FAILED"
    });
    let step = call_step(&addr);
    assert_eq!(step["status"], "failed", "{step}\n{ended}");
    assert!(
        step["error"]
            .as_str()
            .unwrap_or("")
            .starts_with("sign_in_timeout"),
        "{step}"
    );
}

/// A grant the person denies fails the waiting step with the reason.
#[test]
fn a_denied_sign_in_fails_the_step_with_the_reason() {
    let case = Case::new();
    let (d, addr) = case.start("");
    let task = run_fetch(&addr);
    d.wait_log("\"mcp.sign_in.required\"", 1, 20);
    case.idp.set(DENIED);
    wait_task(&addr, &task, 30, "the run's failure", |t| {
        state(t) == "TASK_STATE_FAILED"
    });
    let log = d.wait_log("\"mcp.sign_in.failed\"", 1, 5);
    assert!(log.contains("\"reason\":\"denied\""), "{log}");
    let step = call_step(&addr);
    assert!(
        step["error"]
            .as_str()
            .unwrap_or("")
            .starts_with("sign_in_denied"),
        "{step}"
    );
}

/// A restart in the middle of the wait: the step still waits, a new
/// sign-in starts, and the step completes once the person signs in.
#[test]
fn a_restart_mid_wait_signs_in_again_and_completes() {
    let case = Case::new();
    let (mut d, addr) = case.start("");
    let task = run_fetch(&addr);
    d.wait_log("\"mcp.sign_in.required\"", 1, 20);
    wait_task(&addr, &task, 10, "the sign-in gate", |t| {
        state(t) == "TASK_STATE_INPUT_REQUIRED"
    });
    d.kill();
    let first = case.idp.devices.load(Ordering::SeqCst);

    let (d, addr) = case.start("");
    d.wait_log("\"mcp.sign_in.required\"", 1, 20);
    assert!(case.idp.devices.load(Ordering::SeqCst) > first);
    case.idp.set(APPROVED);
    wait_task(&addr, &task, 30, "the run's end after the restart", |t| {
        state(t) == "TASK_STATE_COMPLETED"
    });
    assert_eq!(call_step(&addr)["status"], "done");
}

// SPDX-License-Identifier: Apache-2.0
//! The three A2A workflow nodes that used to be refused at parse time:
//! the `a2a` START node, `a2a.send` and `a2a.wait` (RFC 0027 §5).
//!
//! What they add is the ASYNCHRONOUS half of an A2A conversation.
//! `a2a.delegate` was the only implemented one, and it is request/response: it
//! blocks a step until the peer produces a result. That cannot express "a peer
//! asked me to do something" (inbound became a conversational turn, never a
//! run) or "tell a peer and carry on, the answer comes later".
//!
//! These tests drive a real daemon over a real A2A listener.

mod common;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Daemon {
    child: Child,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}

fn spawn(config: &str) -> Daemon {
    let stderr_path = common::unique_path("a2a-nodes", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn agentd");
    Daemon { child, stderr_path }
}

/// Readiness is `proc.ready`, not an open socket: the listener binds before the
/// workflow registry is populated, so a message that names a command can
/// otherwise arrive before the start node that would match it exists.
fn wait_ready(addr: &str, d: &Daemon) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if TcpStream::connect(addr).is_ok() && d.stderr().contains("\"event\":\"proc.ready\"") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never became ready:\n{}",
            d.stderr()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn post(addr: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).expect("connect a2a");
    s.set_read_timeout(Some(Duration::from_secs(30))).ok();
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

fn send_message(addr: &str, parts: Value, ctx: Option<&str>) -> Value {
    let mut message = json!({"messageId": "m-1", "role": "ROLE_USER", "parts": parts});
    if let Some(c) = ctx {
        message["contextId"] = json!(c);
    }
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage",
                      "params": {"message": message}})
    .to_string();
    serde_json::from_str(&post(addr, &body)).unwrap_or(Value::Null)
}

fn wait_for(d: &Daemon, needle: &str, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if d.stderr().contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// An inbound COMMAND fires a workflow run instead of becoming a conversational
/// turn. This is the whole point of the `a2a` start node: before it, a peer
/// could only ever talk to the agent, never ask it to run something (short of
/// the built-in `workflow.run` command).
#[test]
fn an_a2a_start_node_turns_an_inbound_command_into_a_run() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-start", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"2\"\n\
             agent:\n  name: a2a-nodes\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: reviewer\n\
             \x20   steps:\n\
             \x20     trigger: {{kind: a2a, command: \"review.start\"}}\n\
             \x20     work:    {{kind: noop, depends_on: [trigger]}}\n\
             \x20     fin:     {{kind: finish, depends_on: [work], status: completed}}\n"
        ),
    )
    .unwrap();
    let d = spawn(&cfg);
    wait_ready(&addr, &d);

    let resp = send_message(
        &addr,
        json!([{"data": {"agentd": {"op": "review.start"}}}]),
        Some("conv-a"),
    );
    assert!(
        resp.get("error").is_none(),
        "the command was refused: {resp}"
    );

    assert!(
        wait_for(&d, "\"event\":\"start.a2a.fired\"", 15),
        "the a2a start node did not fire:\n{}",
        d.stderr()
    );
    assert!(
        wait_for(&d, "\"event\":\"run.done\"", 20),
        "the run did not complete:\n{}",
        d.stderr()
    );
    let _ = std::fs::remove_file(&cfg);
}

/// A message whose command does NOT match any start node keeps its old
/// behaviour — it is a conversation, not a trigger. The router must narrow, not
/// swallow: a start node that matched everything would silently stop the agent
/// answering anyone.
#[test]
fn a_non_matching_message_is_still_a_conversation() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-nomatch", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"2\"\n\
             agent:\n  name: a2a-nodes\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: reviewer\n\
             \x20   steps:\n\
             \x20     trigger: {{kind: a2a, command: \"review.start\"}}\n\
             \x20     fin:     {{kind: finish, depends_on: [trigger], status: completed}}\n"
        ),
    )
    .unwrap();
    let d = spawn(&cfg);
    wait_ready(&addr, &d);

    // A different command: must NOT fire the start node.
    send_message(
        &addr,
        json!([{"data": {"agentd": {"op": "status"}}}]),
        Some("conv-b"),
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !d.stderr().contains("\"event\":\"start.a2a.fired\""),
        "a non-matching command fired the start node:\n{}",
        d.stderr()
    );
    let _ = std::fs::remove_file(&cfg);
}

/// `a2a.wait` is woken by an arriving message rather than only by its timeout.
///
/// This is the half that did not exist: `wait {on: message}` suspended on a
/// conversation and nothing ever resolved it, so it could only ever time out.
/// The workflow below suspends immediately and must complete as soon as a
/// message lands on its conversation — well inside the generous timeout.
#[test]
fn an_a2a_wait_is_woken_by_the_message_it_waits_for() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-wait", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"2\"\n\
             agent:\n  name: a2a-nodes\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: awaiter\n\
             \x20   steps:\n\
             \x20     go:    {{kind: once}}\n\
             \x20     reply: {{kind: a2a.wait, depends_on: [go], conversation: \"conv-w\", timeout: 10m}}\n\
             \x20     fin:   {{kind: finish, depends_on: [reply], status: completed}}\n"
        ),
    )
    .unwrap();
    let d = spawn(&cfg);
    wait_ready(&addr, &d);
    // The run starts itself (`once`) and parks on the wait.
    assert!(
        wait_for(&d, "\"event\":\"run.start\"", 15),
        "the run never started:\n{}",
        d.stderr()
    );
    assert!(
        !d.stderr().contains("\"event\":\"run.done\""),
        "the run finished before the message arrived:\n{}",
        d.stderr()
    );

    // Now say something on that conversation.
    send_message(
        &addr,
        json!([{"text": "here is your answer"}]),
        Some("conv-w"),
    );

    assert!(
        wait_for(&d, "\"event\":\"a2a.message.delivered\"", 15),
        "the waiting step was not woken:\n{}",
        d.stderr()
    );
    assert!(
        wait_for(&d, "\"event\":\"run.done\"", 20),
        "the run did not complete after being woken:\n{}",
        d.stderr()
    );
    let _ = std::fs::remove_file(&cfg);
}

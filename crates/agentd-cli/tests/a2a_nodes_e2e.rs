// SPDX-License-Identifier: AGPL-3.0-only
//! The three A2A workflow nodes, from parse through dispatch: the `a2a` START
//! node, `a2a.send` and `a2a.wait`.
//!
//! What they add is the ASYNCHRONOUS half of an A2A conversation.
//! `a2a.delegate` is request/response: it blocks a step until the peer produces
//! a result. That cannot express "a peer asked me to do something" (which would
//! otherwise be only a conversational turn, never a run) or "tell a peer and
//! carry on, the answer comes later".
//!
//! These tests drive a real daemon over a real A2A listener.

// Both features are load-bearing, not incidental: without `a2a` there is no
// listener to receive a message, and without `workflow` the configs below do
// not load at all. Ungated, this file compiles into every feature combination
// the CI matrix builds and fails each one at `wait_ready` — a daemon that never
// becomes ready because the surface under test was never built.
#![cfg(all(unix, feature = "a2a", feature = "workflow"))]

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
/// turn. This is the whole point of the `a2a` start node: without one, a peer
/// can only ever talk to the agent, never ask it to run something (short of the
/// built-in `workflow.run` command).
#[test]
fn an_a2a_start_node_turns_an_inbound_command_into_a_run() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-start", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
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

/// A message whose command does NOT match any start node is still a
/// conversation, not a trigger. The router must narrow, not swallow: a start
/// node that matched everything would silently stop the agent answering anyone.
#[test]
fn a_non_matching_message_is_still_a_conversation() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-nomatch", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
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
/// The failure this guards against: a `wait {on: message}` that suspends on a
/// conversation with nothing to resolve it can only ever time out. The workflow
/// below suspends immediately and must complete as soon as a message lands on
/// its conversation — well inside the generous timeout.
#[test]
fn an_a2a_wait_is_woken_by_the_message_it_waits_for() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = common::unique_path("a2a-wait", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
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

/// **An `a2a` start with `into:` appends the message to a stream** (RFC 0035 §5).
///
/// The peer-facing counterpart of the webhook binding: a fleet peer feeds a
/// durable stream over the A2A channel instead of firing a run per message, so
/// the same replay-after-downtime applies to peer traffic. Authorization is
/// unchanged — the principal is resolved and any `roles` filter applied before
/// the append — so this is a different destination for an accepted message, not
/// a way around the gate.
#[test]
fn an_a2a_start_can_append_its_command_to_a_stream_instead_of_running() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let dir = common::unique_path("a2a-into", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = common::unique_path("a2a-into", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: a2a-into\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  peers:\n    retention: {{ max_events: 100 }}\n\
             a2a:\n  listen: http://127.0.0.1:{port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n  log_content: true\n\
             workflows:\n\
             \x20 - name: intake\n\
             \x20   steps:\n\
             \x20     trigger: {{kind: a2a, command: \"telemetry.report\",\n\
             \x20                into: {{stream: peers, subject: \"peer.telemetry\"}}}}\n\
             \x20 - name: drain\n\
             \x20   steps:\n\
             \x20     take: {{kind: stream, stream: peers, subject: \"peer.*\", from: earliest}}\n\
             \x20     note: {{kind: assign, depends_on: [take], value: \"drained {{{{steps.take.output.subject}}}}\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [note], status: completed, output: \"{{{{steps.note.output}}}}\"}}\n"
        ),
    )
    .unwrap();
    let d = spawn(&cfg);
    wait_ready(&addr, &d);

    let resp = send_message(
        &addr,
        json!([{"data": {"agentd": {"op": "telemetry.report"}}}]),
        Some("conv-into"),
    );
    assert!(
        resp.get("error").is_none(),
        "the command was refused: {resp}"
    );

    assert!(
        wait_for(&d, "\"event\":\"start.a2a.into\"", 15),
        "the message was appended to the stream:\n{}",
        d.stderr()
    );
    // It appended INSTEAD of running its own workflow…
    assert!(
        !d.stderr().contains("\"event\":\"start.a2a.fired\""),
        "an `into` start does not also fire a run:\n{}",
        d.stderr()
    );
    // …and the stream consumer picked it up.
    assert!(
        wait_for(&d, "drained peer.telemetry", 15),
        "the appended event reached the consumer:\n{}",
        d.stderr()
    );

    std::fs::remove_file(&cfg).ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// **Cross-instance streaming**: one agent's `emit … forward: {peer:}` becomes
/// another agent's stream event, via that peer's `a2a` start with `into:`.
///
/// This is the whole point of the two Phase C bindings meeting: A emits to its
/// own durable stream and forwards the append to B; B accepts the message
/// through its ordinary A2A authorization and appends it to ITS stream, where
/// B's consumer picks it up. Each side keeps an independent durable copy — the
/// forward is the notification, not the delivery.
#[test]
fn an_emit_forwarded_to_a_peer_lands_on_that_peers_stream() {
    let b_port = free_port();
    let b_addr = format!("127.0.0.1:{b_port}");
    let dir_b = common::unique_path("fwd-b", "d");
    let dir_a = common::unique_path("fwd-a", "d");
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::create_dir_all(&dir_a).unwrap();

    // B: accepts the forwarded command and binds it onto its own stream.
    let cfg_b = common::unique_path("fwd-b", "yaml");
    std::fs::write(
        &cfg_b,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: receiver\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: file\n  file:\n    path: {dir_b}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  incoming:\n    retention: {{ max_events: 100 }}\n\
             a2a:\n  listen: http://127.0.0.1:{b_port}\n\
             lifecycle:\n  run_until: drained\n\
             observability:\n  log_level: info\n  log_content: true\n\
             workflows:\n\
             \x20 - name: intake\n\
             \x20   steps:\n\
             \x20     t: {{kind: a2a, command: \"stream.forwarded\",\n\
             \x20          into: {{stream: incoming, subject: \"from.peer\"}}}}\n\
             \x20 - name: drain\n\
             \x20   steps:\n\
             \x20     take: {{kind: stream, stream: incoming, subject: \"from.*\", from: earliest}}\n\
             \x20     note: {{kind: assign, depends_on: [take], value: \"peer sent {{{{steps.take.output.data.args.subject}}}}\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [note], status: completed, output: \"{{{{steps.note.output}}}}\"}}\n"
        ),
    )
    .unwrap();
    let b = spawn(&cfg_b);
    wait_ready(&b_addr, &b);

    // A: emits to its own stream and forwards the append to B.
    let cfg_a = common::unique_path("fwd-a", "yaml");
    std::fs::write(
        &cfg_a,
        format!(
            "config_version: \"1\"\n\
             agent:\n  name: sender\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: file\n  file:\n    path: {dir_a}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  outbox:\n    retention: {{ max_events: 100 }}\n\
             a2a:\n  peers:\n    - name: receiver\n      endpoint: http://127.0.0.1:{b_port}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
             observability:\n  log_level: info\n  log_content: true\n\
             workflows:\n\
             \x20 - name: producer\n\
             \x20   steps:\n\
             \x20     s: {{kind: once, policy: always}}\n\
             \x20     e: {{kind: emit, depends_on: [s], stream: outbox, subject: \"order.placed\",\n\
             \x20         data: {{n: 1}}, forward: {{peer: receiver}}}}\n\
             \x20     f: {{kind: finish, depends_on: [e], status: completed}}\n"
        ),
    )
    .unwrap();
    let a = spawn(&cfg_a);

    assert!(
        wait_for(&a, "\"event\":\"stream.emit\"", 15),
        "A appended to its own stream:\n{}",
        a.stderr()
    );
    assert!(
        wait_for(&b, "\"event\":\"start.a2a.into\"", 20),
        "B received the forward and appended it:\n{}",
        b.stderr()
    );
    assert!(
        wait_for(&b, "peer sent order.placed", 20),
        "B's own consumer drained the forwarded event:\n{}",
        b.stderr()
    );

    std::fs::remove_file(&cfg_a).ok();
    std::fs::remove_file(&cfg_b).ok();
    std::fs::remove_dir_all(&dir_a).ok();
    std::fs::remove_dir_all(&dir_b).ok();
}

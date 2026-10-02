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
// the CI matrix builds and fails each one at `spawn_bound` — a daemon that never
// becomes ready because the surface under test was never built.
#![cfg(all(unix, feature = "a2a"))]

mod common;

use serde_json::json;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::SendMessage;

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
    let cfg = common::unique_path("a2a-start", "yaml");
    let (d, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            format!(
                "\
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
        let log = d.stderr_path.clone();
        (d, log)
    });

    let resp = SendMessage::command("review.start", json!({}))
        .context("conv-a")
        .post(&addr);
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
    let cfg = common::unique_path("a2a-nomatch", "yaml");
    let (d, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            format!(
                "\
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
        let log = d.stderr_path.clone();
        (d, log)
    });

    // A different command: must NOT fire the start node.
    let _ = SendMessage::command("status", json!({}))
        .context("conv-b")
        .post_raw(&addr);
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
/// its conversation — well inside the generous timeout. The sender is the
/// loopback listener's implicit operator, which is who the wait's `from`
/// names.
#[test]
fn an_a2a_wait_is_woken_by_the_message_it_waits_for() {
    let cfg = common::unique_path("a2a-wait", "yaml");
    let (d, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            format!(
                "\
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
                 \x20     reply: {{kind: a2a.wait, depends_on: [go], conversation: \"conv-w\", from: operator, timeout: 10m}}\n\
                 \x20     fin:   {{kind: finish, depends_on: [reply], status: completed}}\n"
            ),
        )
        .unwrap();
        let d = spawn(&cfg);
        let log = d.stderr_path.clone();
        (d, log)
    });
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
    let _ = SendMessage::text("here is your answer")
        .context("conv-w")
        .post_raw(&addr);

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
    let dir = common::unique_path("a2a-into", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = common::unique_path("a2a-into", "yaml");
    let (d, addr) = common::spawn_bound(|port| {
        std::fs::write(
            &cfg,
            format!(
                "\
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
        let log = d.stderr_path.clone();
        (d, log)
    });

    let resp = SendMessage::command("telemetry.report", json!({}))
        .context("conv-into")
        .post(&addr);
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
    let dir_b = common::unique_path("fwd-b", "d");
    let dir_a = common::unique_path("fwd-a", "d");
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::create_dir_all(&dir_a).unwrap();

    // B: accepts the forwarded command and binds it onto its own stream.
    let cfg_b = common::unique_path("fwd-b", "yaml");
    let (b, b_addr) = common::spawn_bound(|b_port| {
        std::fs::write(
            &cfg_b,
            format!(
                "\
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
        let log = b.stderr_path.clone();
        (b, log)
    });

    // A: emits to its own stream and forwards the append to B.
    let cfg_a = common::unique_path("fwd-a", "yaml");
    std::fs::write(
        &cfg_a,
        format!(
            "\
             agent:\n  name: sender\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: file\n  file:\n    path: {dir_a}/state\n  checkpoint:\n    debounce_ms: 0\n\
             streams:\n  outbox:\n    retention: {{ max_events: 100 }}\n\
             a2a:\n  peers:\n    - name: receiver\n      endpoint: http://{b_addr}\n\
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
        "B received the forward and appended it:\n{}\n--- A (the sender) ---\n{}",
        b.stderr(),
        a.stderr()
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

/// The daemon's events named `event`, parsed from its JSON log.
fn events(d: &Daemon, event: &str) -> Vec<serde_json::Value> {
    d.stderr()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|e| e["event"] == event)
        .collect()
}

/// **A plain-text `a2a.send` reaches an agentd peer.**
///
/// A conversational message goes through the same typed parse any peer's
/// would — an A2A 1.0 server refuses a role spelled any other way than
/// `ROLE_USER` — and B's waiting step is woken by it.
#[test]
fn plain_text_send_to_agentd_peer() {
    let cfg_b = common::unique_path("send-b", "yaml");
    let (b, b_addr) = common::spawn_bound(|b_port| {
        std::fs::write(
            &cfg_b,
            format!(
                "\
                 agent:\n  name: listener\n  instruction: test\n  preflight: never\n\
                 intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
                 store:\n  kind: memory\n\
                 a2a:\n  listen: http://127.0.0.1:{b_port}\n\
                 lifecycle:\n  run_until: drained\n\
                 observability:\n  log_level: info\n  log_content: true\n\
                 workflows:\n\
                 \x20 - name: awaiter\n\
                 \x20   steps:\n\
                 \x20     go:    {{kind: once}}\n\
                 \x20     reply: {{kind: a2a.wait, depends_on: [go], conversation: \"conv-p\", from: operator, timeout: 10m}}\n\
                 \x20     fin:   {{kind: finish, depends_on: [reply], status: completed, output: \"heard {{{{steps.reply.output.message.text}}}}\"}}\n"
            ),
        )
        .unwrap();
        let b = spawn(&cfg_b);
        let log = b.stderr_path.clone();
        (b, log)
    });
    assert!(
        wait_for(&b, "\"event\":\"run.start\"", 15),
        "B's run never started:\n{}",
        b.stderr()
    );
    // The run parks on its wait just after it starts; a message that lands
    // before the wait exists is a conversational turn instead.
    std::thread::sleep(Duration::from_millis(500));

    let cfg_a = common::unique_path("send-a", "yaml");
    std::fs::write(
        &cfg_a,
        format!(
            "\
             agent:\n  name: sender\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  peers:\n    - name: b\n      endpoint: http://{b_addr}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
             observability:\n  log_level: info\n\
             workflows:\n\
             \x20 - name: teller\n\
             \x20   steps:\n\
             \x20     s:    {{kind: once, policy: always}}\n\
             \x20     tell: {{kind: a2a.send, depends_on: [s], to: b, parts: \"hello from A\", context: \"conv-p\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [tell], status: completed}}\n"
        ),
    )
    .unwrap();
    let a = spawn(&cfg_a);

    assert!(
        wait_for(&a, "\"event\":\"run.done\"", 20),
        "A's run never finished:\n{}",
        a.stderr()
    );
    let tell = events(&a, "step.done")
        .into_iter()
        .find(|e| e["step"] == "tell")
        .unwrap_or_else(|| panic!("A logged no step.done for the send:\n{}", a.stderr()));
    assert_eq!(
        tell["status"],
        "done",
        "B refused the send: {tell}\nB:\n{}",
        b.stderr()
    );
    assert!(
        wait_for(&b, "\"event\":\"a2a.message.delivered\"", 15),
        "B's waiting step was not woken:\n{}",
        b.stderr()
    );
    assert!(
        wait_for(&b, "heard hello from A", 15),
        "B's run did not read A's text:\n{}",
        b.stderr()
    );

    std::fs::remove_file(&cfg_a).ok();
    std::fs::remove_file(&cfg_b).ok();
}

/// One request the fixture peer below received.
struct PeerRequest {
    head: String,
    body: serde_json::Value,
}

/// **Delegating to a peer that does not stream.**
///
/// A 1.0 peer whose card does not claim `streaming` must answer the streaming
/// methods with UnsupportedOperation, so a client that always opened a stream
/// failed every delegation to it. The peer here is a spec-shaped fixture: it
/// publishes a card without streaming, refuses `SendStreamingMessage` with
/// `-32004`, and answers `SendMessage` with a working task it completes on the
/// first `GetTask`. agentd must read the card, send unary with
/// `returnImmediately`, poll, and hand the step the artifact.
#[test]
fn delegation_to_non_streaming_peer() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::sync::{Arc, Mutex};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let peer_url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<PeerRequest>>> = Arc::default();
    {
        let seen = Arc::clone(&seen);
        let card = json!({
            "name": "plain-peer",
            "description": "a peer that does not stream",
            "version": "1",
            "supportedInterfaces": [
                {"url": format!("{peer_url}/"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}
            ],
            "capabilities": {"streaming": false},
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": [],
        });
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                s.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut head = String::new();
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.trim().eq_ignore_ascii_case("content-length")
                    {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&line);
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                let id = body["id"].clone();
                let reply = if head.starts_with("GET /.well-known/agent-card.json ") {
                    card.to_string()
                } else {
                    let result = |r: serde_json::Value| {
                        json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string()
                    };
                    match body["method"].as_str() {
                        Some("SendMessage") => result(json!({"task": {
                            "id": "np-1", "contextId": "np-ctx",
                            "status": {"state": "TASK_STATE_WORKING"}
                        }})),
                        Some("GetTask") => result(json!({
                            "id": "np-1", "contextId": "np-ctx",
                            "status": {"state": "TASK_STATE_COMPLETED"},
                            "artifacts": [{"artifactId": "np-1.result", "parts": [{"text": "non-streaming answer"}]}]
                        })),
                        _ => json!({"jsonrpc": "2.0", "id": id, "error": {
                            "code": -32004, "message": "streaming is not supported"
                        }})
                        .to_string(),
                    }
                };
                seen.lock().unwrap().push(PeerRequest { head, body });
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    )
                    .as_bytes(),
                );
            }
        });
    }

    let cfg = common::unique_path("np-delegate", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "\
             agent:\n  name: delegator\n  instruction: test\n  preflight: never\n\
             intelligence:\n  endpoints: http://127.0.0.1:1/v1\n  model: mock\n\
             store:\n  kind: memory\n\
             a2a:\n  peers:\n    - name: plain\n      endpoint: {peer_url}\n\
             lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
             observability:\n  log_level: info\n  log_content: true\n\
             workflows:\n\
             \x20 - name: asker\n\
             \x20   steps:\n\
             \x20     s:    {{kind: once, policy: always}}\n\
             \x20     del:  {{kind: a2a.delegate, depends_on: [s], peer: plain, objective: \"answer plainly\", timeout: 30s}}\n\
             \x20     note: {{kind: assign, depends_on: [del], value: \"peer said {{{{steps.del.output}}}}\"}}\n\
             \x20     f:    {{kind: finish, depends_on: [note], status: completed, output: \"{{{{steps.note.output}}}}\"}}\n"
        ),
    )
    .unwrap();
    let d = spawn(&cfg);
    assert!(
        wait_for(&d, "peer said non-streaming answer", 20),
        "the delegation did not complete through the unary path:\n{}",
        d.stderr()
    );

    let seen = seen.lock().unwrap();
    let calls: Vec<String> = seen
        .iter()
        .map(|r| {
            r.body["method"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| r.head.lines().next().unwrap_or("").to_string())
        })
        .collect();
    assert!(
        !calls.iter().any(|c| c == "SendStreamingMessage"),
        "the card said the peer does not stream: {calls:?}"
    );
    assert!(
        calls[0].starts_with("GET /.well-known/agent-card.json"),
        "the card is read first: {calls:?}"
    );
    let send = seen
        .iter()
        .find(|r| r.body["method"] == "SendMessage")
        .expect("a unary send");
    assert_eq!(send.body["params"]["message"]["role"], "ROLE_USER");
    assert_eq!(
        send.body["params"]["configuration"]["returnImmediately"],
        true
    );
    assert!(calls.iter().any(|c| c == "GetTask"), "{calls:?}");
    for r in seen.iter() {
        assert!(
            r.head.to_ascii_lowercase().contains("a2a-version: 1.0\r\n"),
            "every request states the version:\n{}",
            r.head
        );
    }
    std::fs::remove_file(&cfg).ok();
}

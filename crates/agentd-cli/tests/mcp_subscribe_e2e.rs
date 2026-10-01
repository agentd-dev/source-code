// SPDX-License-Identifier: AGPL-3.0-only
//! A resource subscription, end to end, through the two ways it used to go
//! quiet without a word.
//!
//! - **The server restarts.** It forgets the session, and every subscription
//!   the session held goes with it. The daemon learns of it from the `404` its
//!   notification stream's redial gets, re-dials, says so (`mcp.disconnect`
//!   then `mcp.connect`, both `reason: session_lost`), and subscribes again —
//!   counted on the server, and proved by a wake that fires a run on the new
//!   session.
//! - **The server offers no subscriptions.** A `subscribe` start that can never
//!   fire is `start.subscribe.unsupported` at error level, and a resource wait
//!   fails its step, instead of parking for ever — whether the server never
//!   offered them or came back from a restart without them.
//!
//! What a re-dial restores is read off the owners — the starts, the waits, the
//! instruction — through the code a reload's re-dial takes, and a subscribe it
//! makes that fails in a way that may pass is asked again.
#![cfg(unix)]

mod common;

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WATCHED: &str = "mock://watched";

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A daemon on `config`, its stderr in a file the test reads as it goes.
struct Daemon {
    child: Child,
    err_path: String,
    cfg_path: String,
}

impl Daemon {
    fn start(tag: &str, config: &str) -> Daemon {
        let cfg_path = common::unique_path(tag, "yaml");
        std::fs::write(&cfg_path, config).expect("write config");
        let err_path = common::unique_path(tag, "err");
        let err = std::fs::File::create(&err_path).expect("stderr file");
        let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &cfg_path])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn agentd");
        Daemon {
            child,
            err_path,
            cfg_path,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    fn wait_for(&self, what: &str, pred: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let log = self.log();
            if pred(&log) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{log}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.err_path);
        let _ = std::fs::remove_file(&self.cfg_path);
    }
}

/// Call one of the mock's own tools with no session, as an operator's script
/// would — the mock serves a request that carries none.
fn mock_tool(endpoint: &str, tool: &str) {
    mock_tool_with(endpoint, tool, "{}");
}

/// [`mock_tool`] with `arguments` (a JSON object).
fn mock_tool_with(endpoint: &str, tool: &str, arguments: &str) {
    let addr = endpoint.trim_start_matches("http://");
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{arguments}}}}}"#
    );
    let mut s = TcpStream::connect(addr).expect("dial mock");
    write!(
        s,
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("send");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 200"), "mock tool {tool}: {out}");
}

#[test]
fn a_server_that_forgets_the_session_is_re_dialed_and_the_subscription_restored() {
    let mock = common::spawn_mock_mcp(WATCHED, true);
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "mcp-session-lost",
        &format!(
            "agent:\n  name: session-lost\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );

    // Subscribed, and the mock's one push after a subscribe fired a run.
    daemon.wait_for("the first wake to fire a run", |log| {
        !events(log, "run.done").is_empty()
    });
    assert_eq!(mock.subscribes(WATCHED), 1);

    // The server restarts: its session, and the subscription with it, gone.
    mock_tool(&endpoint, "mock.restart");

    daemon.wait_for("a run fired by a wake on the new session", |log| {
        events(log, "run.done").len() >= 2
    });
    let log = daemon.log();
    let lost = events(&log, "mcp.disconnect");
    assert!(
        lost.iter()
            .any(|e| e["server"] == "mock" && e["reason"] == "session_lost"),
        "the loss is said:\n{log}"
    );
    let back = events(&log, "mcp.connect");
    assert!(
        back.iter()
            .any(|e| e["server"] == "mock" && e["reason"] == "session_lost"),
        "the re-dial is said:\n{log}"
    );
    assert!(
        events(&log, "start.subscribe.armed")
            .iter()
            .any(|e| e["uri"] == WATCHED && e["reason"] == "session_lost"),
        "the subscription is restored:\n{log}"
    );
    // Counted where it lands: the server saw the subscription made again.
    assert_eq!(mock.subscribes(WATCHED), 2, "{}", mock.log());
}

#[test]
fn a_server_without_subscriptions_is_a_loud_failure_not_a_silent_park() {
    let addr_file = common::unique_path("mock-nosub", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let thread_file = addr_file.clone();
    std::thread::spawn(move || {
        agentd::mcp::mock_http::run_offering(&thread_file, WATCHED, true, false);
    });
    let endpoint = format!("http://{}/mcp", common::read_addr_file(&addr_file));
    let daemon = Daemon::start(
        "mcp-nosub",
        &format!(
            "agent:\n  name: nosub\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n  - name: waits\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: resource, server: mock, uri: \"{WATCHED}\", depends_on: [s]}}\n      f: {{kind: finish, depends_on: [w], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );

    daemon.wait_for("the start to be refused and the wait to fail", |log| {
        !events(log, "start.subscribe.unsupported").is_empty()
            && log.contains("wait resource: subscribe")
    });
    let log = daemon.log();
    let refused = &events(&log, "start.subscribe.unsupported")[0];
    assert_eq!(refused["level"], "error", "{log}");
    assert_eq!(refused["uri"], WATCHED, "{log}");
    assert!(
        events(&log, "start.subscribe.armed").is_empty(),
        "nothing was armed:\n{log}"
    );
    assert!(
        log.contains("resources.subscribe"),
        "the wait's failure names the missing capability:\n{log}"
    );
    let _ = std::fs::remove_file(&addr_file);
}

/// The two workflows the restart tests watch with: a `subscribe` start on
/// [`WATCHED`] and a `wait {on: resource}` on `mock://waited`, each counted
/// apart at the server.
fn watch_and_wait(endpoint: &str, name: &str) -> String {
    format!(
        "agent:\n  name: {name}\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: watch\n    steps:\n      s: {{kind: subscribe, server: mock, uri: \"{WATCHED}\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\n  - name: waits\n    steps:\n      s: {{kind: once}}\n      w: {{kind: wait, on: resource, server: mock, uri: \"mock://waited\", depends_on: [s], timeout: 120s}}\n      f: {{kind: finish, depends_on: [w], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
    )
}

#[test]
fn a_server_back_from_a_restart_without_subscriptions_fails_the_waits_and_says_the_start() {
    // The re-dial restores through the code boot and a reload take, so a
    // server that came back without `resources.subscribe` gets the answer
    // it gets there — not a warning and a wait parked for ever.
    let mock = common::spawn_mock_mcp(WATCHED, false);
    let endpoint = mock.uri();
    let daemon = Daemon::start("mcp-lost-cap", &watch_and_wait(&endpoint, "lostcap"));
    daemon.wait_for("the start and the wait to subscribe", |_| {
        mock.subscribes(WATCHED) >= 1 && mock.subscribes("mock://waited") >= 1
    });

    mock_tool_with(&endpoint, "mock.restart", r#"{"subscribe": false}"#);

    daemon.wait_for("the wait to fail and the start to be refused", |log| {
        !events(log, "start.subscribe.unsupported").is_empty()
            && events(log, "run.done")
                .iter()
                .any(|e| e["workflow"] == "waits" && e["status"] == "failed")
    });
    let log = daemon.log();
    assert!(
        events(&log, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "error"
                && e["reason"] == "session_lost"
                && e["uri"] == "mock://waited"
                && e["err"]
                    .as_str()
                    .is_some_and(|m| m.contains("resources.subscribe"))),
        "the wait's failure names the lost capability:\n{log}"
    );
    let refused = &events(&log, "start.subscribe.unsupported")[0];
    assert_eq!(refused["level"], "error", "{log}");
    assert_eq!(refused["uri"], WATCHED, "{log}");
}

#[test]
fn a_re_subscribe_the_restarted_server_refuses_is_asked_again() {
    // A server still warming up refuses the first subscribe on the new
    // session. Said once and left, the start never fired again.
    let mock = common::spawn_mock_mcp(WATCHED, true);
    let endpoint = mock.uri();
    let daemon = Daemon::start("mcp-resub-retry", &watch_and_wait(&endpoint, "retry"));
    daemon.wait_for(
        "the first wake to fire a run, and the wait to subscribe",
        |log| !events(log, "run.done").is_empty() && mock.subscribes("mock://waited") >= 1,
    );

    // The start's re-subscribe and the wait's are both refused.
    mock_tool_with(&endpoint, "mock.refuse_subscribe", r#"{"count": 2}"#);
    mock_tool(&endpoint, "mock.restart");

    daemon.wait_for("a run fired by a wake on the retried subscription", |log| {
        events(log, "run.done").len() >= 2
            && events(log, "wait.resubscribed")
                .iter()
                .any(|e| e["reason"] == "retry")
    });
    let log = daemon.log();
    assert!(
        events(&log, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "warn" && e["uri"] == "mock://waited"),
        "{log}"
    );
    assert_eq!(mock.subscribes("mock://waited"), 3, "{}", mock.log());
    assert!(
        events(&log, "start.subscribe.fail")
            .iter()
            .any(|e| e["uri"] == WATCHED),
        "the refusal is said:\n{log}"
    );
    assert!(
        events(&log, "start.subscribe.armed")
            .iter()
            .any(|e| e["uri"] == WATCHED && e["reason"] == "retry"),
        "the start is armed again by the retry:\n{log}"
    );
    // At the server: the first subscribe, the refused one, the retry.
    assert_eq!(mock.subscribes(WATCHED), 3, "{}", mock.log());
}

#[test]
fn a_resource_instruction_is_read_again_after_its_server_restarts() {
    // The publisher may have changed the instruction while the server was
    // down — often why it restarted. Subscribing it again is not enough: an
    // update sent before the new subscription existed reached nobody.
    let mock = common::spawn_mock_mcp(WATCHED, false);
    let endpoint = mock.uri();
    let daemon = Daemon::start(
        "mcp-instruction-redial",
        &format!(
            "agent:\n  name: instructed\n  instruction: mcp://mock/mock://instruction\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}/mcp\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );
    daemon.wait_for("the instruction to be read and subscribed", |_| {
        mock.subscribes("mock://instruction") >= 1
    });
    assert_eq!(mock.reads("mock://instruction"), 1, "{}", mock.log());

    mock_tool(&endpoint, "mock.restart");

    daemon.wait_for("the instruction read again, changed", |log| {
        !events(log, "instruction.updated").is_empty()
    });
    assert_eq!(mock.reads("mock://instruction"), 2, "{}", mock.log());
    assert_eq!(mock.subscribes("mock://instruction"), 2, "{}", mock.log());
}

#[test]
fn a_listen_stream_that_ends_is_said_and_a_narrowed_one_fails_the_wait_it_dropped() {
    // The stateless revision: one `subscriptions/listen` stream carries every
    // subscription. The mock closes each one after its update, so the daemon
    // says the end and listens again; then it starts acknowledging without
    // the waited URI. rmcp drops every update outside an acknowledgment, so
    // the URI is said dropped, asked for again, and — the server still
    // leaving it out — the wait on it fails rather than parking for ever.
    let addr_file = common::unique_path("mock-listen", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let thread_file = addr_file.clone();
    std::thread::spawn(move || {
        agentd::mcp::mock_http::run_listening(&thread_file, WATCHED);
    });
    let endpoint = format!("http://{}", common::read_addr_file(&addr_file));
    let daemon = Daemon::start("mcp-listen", &watch_and_wait(&endpoint, "listener"));
    daemon.wait_for("the listen to end and be opened again", |log| {
        !events(log, "mcp.listen.resumed").is_empty()
    });
    let log = daemon.log();
    let ended = &events(&log, "mcp.listen.ended")[0];
    assert_eq!(ended["server"], "mock", "{log}");
    assert!(ended["retry_ms"].as_u64().is_some(), "{log}");

    mock_tool_with(&endpoint, "mock.narrow", r#"{"uri": "mock://waited"}"#);

    daemon.wait_for("the wait on the dropped URI to fail", |log| {
        events(log, "run.done")
            .iter()
            .any(|e| e["workflow"] == "waits" && e["status"] == "failed")
    });
    let log = daemon.log();
    assert!(
        events(&log, "mcp.listen.narrowed")
            .iter()
            .any(|e| e["dropped"] == serde_json::json!(["mock://waited"])),
        "the narrowing is said:\n{log}"
    );
    assert!(
        events(&log, "wait.resubscribe.fail")
            .iter()
            .any(|e| e["level"] == "error"
                && e["reason"] == "retry"
                && e["err"]
                    .as_str()
                    .is_some_and(|m| m.contains("acknowledged the listen without"))),
        "the retry met the refusal, and the wait was told:\n{log}"
    );
    let _ = std::fs::remove_file(&addr_file);
}

#[test]
fn an_instruction_on_a_server_without_subscriptions_is_said_once_not_on_every_refresh() {
    // Such a server is followed by the refresh re-read alone. Said when the
    // instruction is first read from it — not as a warning on every re-read.
    let addr_file = common::unique_path("mock-nosub-ins", "addr");
    let _ = std::fs::remove_file(&addr_file);
    let thread_file = addr_file.clone();
    std::thread::spawn(move || {
        agentd::mcp::mock_http::run_offering(&thread_file, WATCHED, false, false);
    });
    let endpoint = format!("http://{}/mcp", common::read_addr_file(&addr_file));
    let daemon = Daemon::start(
        "mcp-nosub-instruction",
        &format!(
            "agent:\n  name: refreshed\n  instruction:\n    mcp: \"mcp://mock/mock://instruction\"\n    refresh: 300ms\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {endpoint}\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n"
        ),
    );
    daemon.wait_for("a few refresh re-reads", |log| {
        events(log, "instruction.loaded").len() >= 4
    });
    let log = daemon.log();
    assert!(
        events(&log, "instruction.subscribe.fail").is_empty(),
        "no warning per re-read:\n{log}"
    );
    assert_eq!(
        events(&log, "instruction.subscribe.unsupported").len(),
        1,
        "said once:\n{log}"
    );
    let _ = std::fs::remove_file(&addr_file);
}

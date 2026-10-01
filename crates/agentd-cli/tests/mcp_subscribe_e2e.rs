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
//!   fails its step, instead of parking for ever.
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
    let addr = endpoint.trim_start_matches("http://");
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"{tool}","arguments":{{}}}}}}"#
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
        events(&log, "mcp.resubscribed")
            .iter()
            .any(|e| e["uri"] == WATCHED),
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

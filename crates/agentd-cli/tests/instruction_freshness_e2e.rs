// SPDX-License-Identifier: AGPL-3.0-only
//! **§7.7 revocation, end to end: the freshness watch confirms nothing it did
//! not read.** The real binary follows an `instruction://…` document on the
//! in-tree mock registry, the registry dies mid-run, and the operator's
//! `unavailable:` policy must fire.
//!
//! It did not, for every 1.x release: the MCP SDK's response cache answered
//! each re-read from memory — inside the server's `ttlMs`, and past it whenever
//! the server was down — so the poll logged `instruction.loaded` against a
//! server that no longer existed, the deadline never tripped, and `exit`,
//! `freeze` and `drain` never happened. The mock now offers a `ttlMs` like the
//! registry does, which is what makes that cache reachable here.
//!
//! The arbiter is the SERVER's log, not agentd's: the mock writes one
//! `MOCK_READ <uri>` line per `resources/read` that reached it, and each test
//! compares that with what agentd claims it read.
#![cfg(all(unix, feature = "internal-mocks", feature = "workflow"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const URI: &str = "instruction://ins_mock@stable";

/// Generous: a loaded CI runner is slow, and each wait ends the moment its
/// condition holds.
const WAIT: Duration = Duration::from_secs(20);

struct Daemon {
    child: Child,
    cfg_path: String,
    err_path: String,
}

impl Daemon {
    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    /// Wait until `cond` holds over the log, or panic with the log.
    fn wait_for(&mut self, what: &str, cond: impl Fn(&str) -> bool) -> String {
        let end = Instant::now() + WAIT;
        loop {
            let log = self.log();
            if cond(&log) {
                return log;
            }
            if Instant::now() > end {
                panic!("timed out waiting for {what}:\n{log}");
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!("the daemon exited ({status}) before {what}:\n{log}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the process to exit on its own; its exit code.
    fn wait_exit(&mut self) -> Option<i32> {
        let end = Instant::now() + WAIT;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return status.code();
            }
            if Instant::now() > end {
                panic!("the daemon did not exit:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.cfg_path);
        let _ = std::fs::remove_file(&self.err_path);
    }
}

/// Boot the real daemon on `cfg`, logging to a file, and leave it running so
/// the test can stop the registry underneath it.
fn boot(cfg: Value) -> Daemon {
    let cfg_path = common::unique_path("freshness", "json");
    std::fs::write(&cfg_path, serde_json::to_vec(&cfg).unwrap()).unwrap();
    let err_path = common::unique_path("freshness", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["-c", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(errf)
        .spawn()
        .unwrap();
    Daemon {
        child,
        cfg_path,
        err_path,
    }
}

/// The config every test shares: poll every second, no trust pins (so it is
/// valid with and without `sign`), and a schedule workflow that keeps the
/// instance long-lived and gives a freeze something to refuse.
fn config(instruction: &str, unavailable: &str, servers: Value, every: &str) -> Value {
    json!({
        "config_version": "1",
        "agent": {"name": "freshness", "preflight": "never",
                  "instruction": {"mcp": instruction, "refresh": "1s", "unavailable": unavailable},
                  "document_capabilities": ["compute"]},
        "mcp": {"servers": servers},
        "intelligence": {"endpoints": ["http://127.0.0.1:1/v1"], "model": "mock"},
        "store": {"kind": "memory"},
        "workflows": [{"name": "tick", "steps": {
            "every": {"kind": "schedule", "every": every},
            "done": {"kind": "finish", "depends_on": ["every"], "status": "completed"}
        }}],
    })
}

fn server(name: &str, mock: &common::MockMcp) -> Value {
    json!({"name": name, "endpoint": format!("{}/mcp", mock.uri())})
}

/// Every log line, parsed, in order.
fn lines(log: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn events(log: &str, name: &str) -> Vec<Value> {
    lines(log)
        .into_iter()
        .filter(|v| v["event"] == name)
        .collect()
}

fn loaded(log: &str) -> usize {
    events(log, "instruction.loaded")
        .iter()
        .filter(|e| e["uri"] == URI)
        .count()
}

/// No `instruction.loaded` after the first `instruction.unavailable`: once the
/// server stopped answering, nothing may claim to have read from it.
fn nothing_loaded_after_unavailable(log: &str) {
    let all = lines(log);
    let first = all
        .iter()
        .position(|v| v["event"] == "instruction.unavailable")
        .unwrap_or_else(|| panic!("no instruction.unavailable:\n{log}"));
    assert!(
        !all[first..]
            .iter()
            .any(|v| v["event"] == "instruction.loaded"),
        "a read was reported after the registry died:\n{log}"
    );
}

#[test]
fn a_dead_registry_fails_the_poll_and_the_agent_exits() {
    let mut mock = common::spawn_mock_mcp("mock://watched", false);
    let mut d = boot(config(
        URI,
        "exit",
        json!([server("registry", &mock)]),
        "1h",
    ));

    // Startup plus two polls. Every read agentd reported must have reached
    // the server: with the SDK cache on, the server saw ONE read against
    // three `instruction.loaded`.
    let log = d.wait_for("three reads", |l| loaded(l) >= 3);
    let claimed = loaded(&log);
    assert!(
        mock.reads(URI) >= claimed,
        "agentd logged {claimed} reads, the server saw {}:\n{log}\n--- mock ---\n{}",
        mock.reads(URI),
        mock.log()
    );

    mock.stop();
    std::thread::sleep(Duration::from_millis(200));
    let k0 = loaded(&d.log());

    let code = d.wait_exit();
    let log = d.log();
    assert_eq!(code, Some(6), "exit::MCP_REQUIRED_DOWN:\n{log}");
    let unavailable = events(&log, "instruction.unavailable");
    assert_eq!(unavailable.len(), 1, "{log}");
    let u = &unavailable[0];
    assert_eq!(u["policy"], "exit", "{u}");
    assert_eq!(u["uri"], URI, "the bare uri, as trust pins name it: {u}");
    assert_eq!(u["server"], "registry", "{u}");
    assert!(
        u["err"].as_str().is_some_and(|e| !e.is_empty()),
        "the failure is named: {u}"
    );
    assert!(
        events(&log, "proc.exit").iter().any(|e| e["code"] == 6),
        "{log}"
    );
    nothing_loaded_after_unavailable(&log);
    assert_eq!(loaded(&log), k0, "no read succeeded after the stop:\n{log}");
}

#[test]
fn a_frozen_agent_refuses_new_work_once_its_registry_dies() {
    let mut mock = common::spawn_mock_mcp("mock://watched", false);
    let mut d = boot(config(
        URI,
        "freeze",
        json!([server("registry", &mock)]),
        "400ms",
    ));

    // Work starts while the registry answers.
    let log = d.wait_for("a poll and a scheduled run", |l| {
        loaded(l) >= 2
            && events(l, "start.fired")
                .iter()
                .any(|e| e["workflow"] == "tick")
    });
    assert!(events(&log, "start.frozen").is_empty(), "{log}");

    mock.stop();

    let log = d.wait_for("the freeze to refuse a start", |l| {
        !events(l, "start.frozen").is_empty()
    });
    let unavailable = events(&log, "instruction.unavailable");
    assert_eq!(unavailable.len(), 1, "{log}");
    assert_eq!(unavailable[0]["policy"], "freeze", "{log}");
    assert_eq!(unavailable[0]["server"], "registry", "{log}");
    assert!(
        events(&log, "start.frozen")
            .iter()
            .all(|e| e["workflow"] == "tick"),
        "{log}"
    );
    // Frozen is not dead: live work drains and the daemon keeps watching.
    assert!(
        matches!(d.child.try_wait(), Ok(None)),
        "a freeze must not exit:\n{log}"
    );
    nothing_loaded_after_unavailable(&log);
}

#[test]
fn the_poll_rereads_only_the_server_that_served_the_instruction() {
    // Two servers hold the same document. The instruction came from `a`; once
    // `a` is gone, `b` answering the same uri confirms nothing about `a`.
    let mut a = common::spawn_mock_mcp("mock://watched", false);
    let b = common::spawn_mock_mcp("mock://watched", false);
    let mut d = boot(config(
        &format!("mcp://a/{URI}"),
        "exit",
        json!([server("a", &a), server("b", &b)]),
        "1h",
    ));

    d.wait_for("the startup read and one poll", |l| loaded(l) >= 2);
    a.stop();

    let code = d.wait_exit();
    let log = d.log();
    assert_eq!(code, Some(6), "{log}");
    let unavailable = events(&log, "instruction.unavailable");
    assert_eq!(unavailable.len(), 1, "{log}");
    assert_eq!(unavailable[0]["server"], "a", "{log}");
    assert_eq!(
        b.reads(URI),
        0,
        "the other server was asked to vouch for the dead one:\n{log}\n--- b ---\n{}",
        b.log()
    );
}

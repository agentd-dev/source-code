// SPDX-License-Identifier: AGPL-3.0-only
//! A server's tool list changes while the daemon runs, and a warm subagent —
//! a long-lived child keeping one conversation across many turns — picks the
//! change up at its next turn boundary instead of holding a stale catalogue
//! for its whole life.
//!
//! At both revisions agentd reaches. At 2026-07-28 the only way a
//! `notifications/tools/list_changed` arrives is a `subscriptions/listen`
//! stream that asked for it: every connection to a server that advertises
//! `tools.listChanged` holds one open. At 2025-11-25 it comes down the `GET`
//! stream. The mock's `mock.tools_changed` sends it down every such stream.
//!
//! The turn that starts the helper is what changes the list: through the mock,
//! it waits until the helper is inside its first turn — past the boundary
//! that turn began at — and the three clients that should hear of it (the
//! daemon, the turn worker and the helper) hold their streams open; then it
//! sends the helper its next turn, whose boundary must pick the change up.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The mock LLM with a `file:` playbook; killed on drop.
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
    let pb = common::unique_path("playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("mock-llm", "addr");
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

/// A daemon on `config`, its stderr — the subagents' too, which inherit it —
/// in a file the test reads as it goes. Stopped on drop.
struct Daemon {
    child: Child,
    err_path: String,
    cfg_path: String,
}

impl Daemon {
    fn start(config: &str) -> Daemon {
        let cfg_path = common::unique_path("mcp-list-changed", "yaml");
        std::fs::write(&cfg_path, config).expect("write config");
        let err_path = common::unique_path("mcp-list-changed", "err");
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

#[test]
fn a_tools_list_change_on_a_2026_07_28_server_is_said_and_refreshes_a_warm_subagent() {
    let mock = tools_change_refreshes_a_warm_subagent("2026-07-28");
    assert_eq!(mock.initializes(), 0, "{}", mock.log());
    assert_eq!(mock.refused(), 0, "{}", mock.log());
}

#[test]
fn a_tools_list_change_on_a_2025_11_25_server_is_said_and_refreshes_a_warm_subagent() {
    tools_change_refreshes_a_warm_subagent("2025-11-25");
}

/// Start a warm helper from a turn, change the tool list once the helper is
/// inside its first turn and the daemon, the turn worker and the helper each
/// hold a stream that carries the change, then give the helper its next turn.
/// Returns the mock.
fn tools_change_refreshes_a_warm_subagent(protocol_version: &str) -> common::MockMcp {
    let mock = common::spawn_mock_mcp_at(protocol_version, "mock://noop", false);
    // The turn: start the helper (its handle is in the result); change the
    // list once the helper's first turn has called `search.query` and three
    // clients would hear of it (the mock hands the handle back); then send
    // the helper a message, which is its next turn. The helper — identified
    // by its own system prompt, which the turn's transcript never contains —
    // looks something up in its first turn — by the registry's name for the
    // server's `search.query`, which an agentd contract holds — and after
    // that answers at once.
    let llm = spawn_mock_llm(&json!({
        "turns": [
            {"tool_calls": [{"name": "subagent.run", "arguments": {
                "instruction": "stand by for instructions", "mode": "warm"}}]},
            {"tool_calls": [{"name": "mock.tools_changed",
                "arguments": {"receivers": 3, "after": "search.query"},
                "arguments_from_tool_result": {"handle": "/handle"}}]},
            {"tool_calls": [{"name": "subagent.send", "arguments": {"message": "the tools changed"},
                "arguments_from_tool_result": {"handle": "/handle"}}]},
            {"content": "steered"}
        ],
        "match": [
            {"when_contains": "Result for helper-ready", "content": "standing by"},
            {"when_contains": "You are agentd, an autonomous agent.",
             "tool_calls": [{"name": "mock.search.query", "arguments": {"query": "helper-ready"}}]}
        ]
    }));
    let daemon = Daemon::start(&format!(
        "agent:\n  name: lists\nintelligence:\n  endpoints: {}\n  model: mock\nstore:\n  kind: memory\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}/mcp\nworkflows:\n  - name: steer\n    steps:\n      s: {{kind: once}}\n      a: {{kind: agent, depends_on: [s], instruction: \"start a helper, then steer it\"}}\n      f: {{kind: finish, depends_on: [a], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n",
        llm.uri,
        mock.uri()
    ));

    daemon.wait_for("the warm helper to refresh its tools", |log| {
        !events(log, "mcp.tools_refreshed").is_empty()
    });
    daemon.wait_for("the daemon to say the change", |log| {
        !events(log, "mcp.tools_changed").is_empty()
    });
    let log = daemon.log();
    // All three clients were told, the helper among them.
    let told: Vec<usize> = mock
        .log()
        .lines()
        .filter_map(|l| l.strip_prefix("MOCK_TOOLS_CHANGED "))
        .filter_map(|n| n.parse().ok())
        .collect();
    assert!(told.len() == 1 && told[0] >= 3, "{told:?}\n{}", mock.log());
    assert!(
        events(&log, "mcp.tools_changed")
            .iter()
            .any(|e| e["server"] == "mock"),
        "{log}"
    );
    // The refresh is the warm helper's own, at the boundary of the turn it
    // was sent — its next after the change — and once.
    let helper = events(&log, "subagent.spawn")
        .into_iter()
        .find(|e| e["mode"] == "warm")
        .unwrap_or_else(|| panic!("no warm helper: {log}"));
    let own: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["pid"] == helper["pid"])
        .collect();
    let at = |name: &str| own.iter().position(|e| e["event"] == name);
    let refreshed = at("mcp.tools_refreshed").expect("refreshed");
    assert!(
        at("subagent.inject").is_some_and(|i| i < refreshed),
        "the refresh came at the boundary of the turn the helper was sent:\n{log}"
    );
    assert_eq!(events(&log, "mcp.tools_refreshed").len(), 1, "{log}");
    assert!(
        own[refreshed]["tools"].as_u64().is_some_and(|n| n > 0),
        "{log}"
    );
    mock
}

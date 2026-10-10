// SPDX-License-Identifier: AGPL-3.0-only
//! A configured MCP server that is down at boot joins when it comes up
//! (`runtime::join`): probed with a backoff, then dialed through the reload's
//! own staging, so what names it — here a `subscribe` start — works from then
//! on, with no reload and no restart.
#![cfg(unix)]

mod common;

use serde_json::Value;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

#[test]
fn a_server_down_at_boot_joins_when_it_comes_up() {
    let port = common::free_port();
    let dir = tempfile::tempdir().unwrap();
    let cfg = common::unique_path("join", "yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: joiner\nstore:\n  kind: file\n  file: {{path: {}}}\nmcp:\n  servers:\n    - name: late\n      endpoint: http://127.0.0.1:{port}/mcp\nworkflows:\n  - name: on_change\n    steps:\n      s: {{kind: subscribe, server: late, uri: \"mock://res\"}}\n      f: {{kind: finish, depends_on: [s], status: completed}}\nlifecycle:\n  run_until: drained\nobservability:\n  log_level: info\n",
            dir.path().display()
        ),
    )
    .unwrap();
    let err_path = common::unique_path("join", "err");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&err_path).unwrap())
        .spawn()
        .unwrap();
    let log = || std::fs::read_to_string(&err_path).unwrap_or_default();
    let wait = |what: &str, pred: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let l = log();
            if pred(&l) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}:\n{l}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    wait("the boot dial failing", &|l| {
        !events(l, "mcp.connect.fail").is_empty()
    });
    // The server comes up on its port; nothing is reloaded or restarted.
    let addr = format!("127.0.0.1:{port}");
    let _mock = common::spawn_mock_mcp_with("2026-07-28", "mock://res", true, &["--listen", &addr]);
    wait("the server joined", &|l| {
        !events(l, "mcp.joined").is_empty()
    });
    let l = log();
    assert!(
        events(&l, "mcp.connect")
            .iter()
            .any(|e| e["server"] == "late" && e["reason"] == "joined"),
        "{l}"
    );
    // What names it works: its subscription is made, the update it sends
    // starts a run.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !events(&log(), "run.done")
        .iter()
        .any(|e| e["workflow"] == "on_change" && e["status"] == "completed")
    {
        assert!(
            Instant::now() < deadline,
            "no run from the joined server's update:\n{}\n--- mock ---\n{}",
            log(),
            _mock.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&err_path);
}

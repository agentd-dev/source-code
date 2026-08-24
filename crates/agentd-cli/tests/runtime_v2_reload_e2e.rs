// SPDX-License-Identifier: AGPL-3.0-only
//! agentd daemon lifecycle: a `run_until: drained` instance stays up, applies
//! a SIGHUP reload of the reloadable partition, refuses a restart-only change,
//! and drains cleanly on SIGTERM (exit 0).
#![cfg(all(feature = "hot-reload", unix))]

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn config(instruction: &str, name: &str, llm: &str) -> String {
    format!(
        "config_version: \"1\"\nagent:\n  name: {name}\n  instruction: {instruction}\nintelligence:\n  endpoints: {llm}\n  model: mock\nworkflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\nlifecycle:\n  run_until: drained\n  drain_timeout: 5s\nobservability:\n  log_level: info\n"
    )
}

/// Wait for a log event with the given name; returns the parsed line.
fn wait_event(
    rx: &mpsc::Receiver<serde_json::Value>,
    name: &str,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(v) if v["event"] == name => return v,
            Ok(_) => continue,
            Err(_) => panic!("timed out waiting for {name}"),
        }
    }
}

#[test]
fn a_daemon_reloads_on_sighup_refuses_restart_only_changes_and_drains_on_sigterm() {
    let mock_llm_addr = common::unique_path("mock-llm", "addr");
    let _ = std::fs::remove_file(&mock_llm_addr);
    let mut llm = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &mock_llm_addr, "final"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let llm_uri = format!("http://{}", common::read_addr_file(&mock_llm_addr));
    let cfg_path = common::unique_path("agentd-v2-reload", "yaml");
    std::fs::write(&cfg_path, config("first instruction", "daemon", &llm_uri)).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg_path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd");
    let pid = child.id() as i32;
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel::<serde_json::Value>();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let ready = wait_event(&rx, "proc.ready", Duration::from_secs(20));
    assert_eq!(
        ready["job_shape"], false,
        "a drained daemon is not job-shaped"
    );

    // 1. A reloadable change (the instruction) applies.
    std::fs::write(&cfg_path, config("second instruction", "daemon", &llm_uri)).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let reloaded = wait_event(&rx, "config.reloaded", Duration::from_secs(20));
    let changed: Vec<String> = reloaded["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    assert!(
        changed.contains(&"agent.instruction".to_string()),
        "{changed:?}"
    );

    // 2. A restart-only change (agent.name) is refused; the daemon keeps running.
    std::fs::write(&cfg_path, config("second instruction", "renamed", &llm_uri)).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let refused = wait_event(
        &rx,
        "config.reload.restart_required",
        Duration::from_secs(20),
    );
    assert!(
        refused["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "agent.name"),
        "{refused}"
    );
    assert!(child.try_wait().unwrap().is_none(), "still running");

    // 3. SIGTERM drains and exits 0.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit after SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(0), "clean drain = exit 0");
    let _ = reader.join();
    let _ = llm.kill();
    let _ = llm.wait();
    let _ = std::fs::remove_file(&cfg_path);
}

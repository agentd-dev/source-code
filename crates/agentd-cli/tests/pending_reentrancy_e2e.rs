// SPDX-License-Identifier: Apache-2.0
//! Reentrancy of the deferred-tool table (`poll_pending`): answering one parked
//! request re-enters the reactor, and the cascade can prune `pending` itself —
//! a `race` whose winning branch cancels its losers is the live case. The pass
//! must therefore address entries by target, never by a remembered index: two
//! `workflow.wait` branches with the same deadline resolve in ONE pass, the
//! first reply cancels the other's entry, and a stale index would answer the
//! wrong request or index past the end and panic the reactor thread — which is
//! the whole daemon.
#![cfg(feature = "workflow")]

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("pending-reentrancy", "yaml");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    path
}

fn run_agentd(config: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .output()
        .expect("run agentd")
}

fn events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

#[test]
fn a_race_whose_branches_share_a_deadline_does_not_kill_the_reactor() {
    // Both branches park a `workflow.wait` on the enclosing run (which is by
    // definition still running) with the same short timeout, so both land in
    // one `poll_pending` pass. Replying to the first finishes its branch, the
    // race declares a winner and cancels the loser — pruning the loser's
    // pending entry mid-pass.
    let steps = r#"{
        "start": {"kind": "once"},
        "race": {"kind": "race", "depends_on": ["start"],
                 "branches": {"a": {"steps": {"wait": {"kind": "tool", "name": "workflow.wait", "args": {"run": "{{env.run}}", "timeout": "1s"}}}},
                              "b": {"steps": {"wait": {"kind": "tool", "name": "workflow.wait", "args": {"run": "{{env.run}}", "timeout": "1s"}}}}}},
        "done": {"kind": "finish", "depends_on": ["race"], "status": "completed", "output": {"winner": "{{steps.race.output.winner}}"}}
    }"#;
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: reentrancy\nworkflows:\n  - name: pipe\n    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n"
    ));
    let out = run_agentd(&cfg);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The reactor survived: a panic in it aborts the process (and a stale-index
    // `remove` panics with "removal index ... out of bounds").
    assert!(
        !stderr.contains("panicked"),
        "the reactor thread must not panic:\n{stderr}"
    );
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    let done = events(&stderr, "run.done");
    assert_eq!(done.len(), 1, "one run: {stderr}");
    assert_eq!(done[0]["status"], "completed", "{}", done[0]);
    let winner = done[0]["output"]["winner"].as_str().unwrap_or_default();
    assert!(winner == "a" || winner == "b", "{}", done[0]);
    // Exactly one branch was answered; the loser was cancelled by the winner's
    // reply, not answered afterwards through a shifted index.
    let answered: Vec<String> = events(&stderr, "step.done")
        .iter()
        .filter_map(|e| e["step"].as_str())
        .filter(|s| s.starts_with("race{") && s.ends_with(".wait"))
        .map(str::to_string)
        .collect();
    assert_eq!(
        answered,
        vec![format!("race{{{winner}}}.wait")],
        "only the winning branch's wait is answered: {stderr}"
    );
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Workflow **concurrency overflow** end to end: more start events than
//! `concurrency.max_runs` allows, with `on_overflow: queue` — the default —
//! must park the surplus for a LATER reactor tick and then run it, not re-offer
//! it to the inbox drain that is popping it.
//!
//! The failure these tests guard against: an overflowed event pushed back onto
//! the very deque `process_inbox` is draining, so the drain pops it again
//! immediately, forever. Nothing inside that loop can relieve the cap — only
//! `schedule_runs`, a later step of the same tick, retires a live run — so the
//! single-writer reactor spins at 100% CPU and never reaches its timers,
//! checkpoints or signal handling. Both cases below therefore assert that the
//! process EXITS, under a hard timeout, so that livelock fails fast instead of
//! hanging CI.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a bounded life may take before the test calls it wedged. Generous
/// next to the ~1 s these runs need (the sleep step plus `idle_grace`), because
/// the point is to distinguish "slow" from "never".
const HARD_TIMEOUT: Duration = Duration::from_secs(30);

fn write_file(tag: &str, ext: &str, body: &str) -> String {
    let path = common::unique_path(tag, ext);
    std::fs::write(&path, body).expect("write test file");
    path
}

/// One life of the daemon, killed (and failed) if it does not exit in time.
/// Unlike the other suites' `Command::output()` this never blocks forever: a
/// livelocked reactor must surface as a failing assertion, not a hung run.
fn run_agentd_bounded(config: &str, inbox: &str) -> (Option<i32>, String) {
    let err_path = common::unique_path("agentd-overflow", "err");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .env("AGENTD_TEST_INBOX_FILE", inbox)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");
    let deadline = Instant::now() + HARD_TIMEOUT;
    loop {
        match child.try_wait().expect("wait for agentd") {
            Some(status) => {
                let log = std::fs::read_to_string(&err_path).unwrap_or_default();
                let _ = std::fs::remove_file(&err_path);
                return (status.code(), log);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let log = std::fs::read_to_string(&err_path).unwrap_or_default();
                let _ = std::fs::remove_file(&err_path);
                panic!(
                    "agentd never exited (waited {HARD_TIMEOUT:?}): the inbox drain is livelocked on a queued start event.\nstderr:\n{log}"
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// The telemetry lines named `name`, in emission order.
fn events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The position of the `n`-th (0-based) `name` line among all telemetry lines,
/// so a test can assert that one event happened after another.
fn nth_event_index(stderr: &str, name: &str, n: usize) -> usize {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .enumerate()
        .filter(|(_, v)| v["event"] == name)
        .map(|(i, _)| i)
        .nth(n)
        .unwrap_or_else(|| panic!("no {name}[{n}] line in:\n{stderr}"))
}

/// A capped workflow: a `manual` start (nothing fires on its own), a short
/// durable sleep so a run stays live long enough for the next start event to
/// overflow, and a `finish`.
fn capped_config(concurrency: &str) -> String {
    let steps = r#"{
        "start": {"kind": "manual"},
        "hold": {"kind": "sleep", "depends_on": ["start"], "duration": "400ms"},
        "done": {"kind": "finish", "depends_on": ["hold"], "status": "completed", "output": {"n": "{{inputs.n}}"}}
    }"#;
    write_file(
        "agentd-overflow",
        "yaml",
        &format!(
            "config_version: \"1\"\nagent:\n  name: overflow\nworkflows:\n  - name: capped\n{concurrency}    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n"
        ),
    )
}

fn inbox_for(n: usize) -> String {
    let events: Vec<serde_json::Value> = (1..=n)
        .map(|i| {
            serde_json::json!({"kind": "workflow_run", "payload": {"workflow": "capped", "node": "start", "inputs": {"n": i.to_string()}}})
        })
        .collect();
    write_file(
        "inbox-overflow",
        "json",
        &serde_json::Value::Array(events).to_string(),
    )
}

/// Every run reached a terminal `completed`, exactly once each, and the process
/// exited by itself.
fn assert_all_runs_completed(code: Option<i32>, stderr: &str, expected: usize) {
    assert_eq!(code, Some(0), "stderr:\n{stderr}");
    let started = events(stderr, "run.start");
    assert_eq!(started.len(), expected, "every start event ran:\n{stderr}");
    let done = events(stderr, "run.done");
    assert_eq!(
        done.len(),
        expected,
        "every run reached a terminal status:\n{stderr}"
    );
    assert!(
        done.iter().all(|d| d["status"] == "completed"),
        "no run failed or was dropped: {done:?}"
    );
    let ids: std::collections::BTreeSet<&str> =
        done.iter().filter_map(|d| d["run"].as_str()).collect();
    assert_eq!(ids.len(), expected, "distinct runs: {done:?}");
}

#[test]
fn a_start_event_over_an_explicit_max_runs_cap_runs_on_a_later_tick() {
    // max_runs 1: the second event CANNOT start while the first run holds the
    // slot, so it exercises the queue-overflow path on the very first tick.
    let cfg = capped_config("    concurrency: {max_runs: 1, on_overflow: queue}\n");
    let (code, stderr) = run_agentd_bounded(&cfg, &inbox_for(2));
    assert_all_runs_completed(code, &stderr, 2);
    // The premise: the cap really engaged — run 2 started only after run 1 had
    // finished. Without it the assertions above would hold vacuously on a build
    // that never queued anything.
    assert!(
        nth_event_index(&stderr, "run.start", 1) > nth_event_index(&stderr, "run.done", 0),
        "the second run must wait for the first to finish:\n{stderr}"
    );
}

#[test]
fn start_events_over_the_default_cap_run_on_later_ticks() {
    // No `concurrency:` block at all — pure defaults (max_runs 4, on_overflow
    // queue). Five events: four run, the fifth overflows. This is the shape any
    // workflow gets for free, so a livelock here reaches every deployment that
    // never tunes concurrency at all.
    let cfg = capped_config("");
    let (code, stderr) = run_agentd_bounded(&cfg, &inbox_for(5));
    assert_all_runs_completed(code, &stderr, 5);
    assert!(
        nth_event_index(&stderr, "run.start", 4) > nth_event_index(&stderr, "run.done", 0),
        "the fifth run must wait for a slot:\n{stderr}"
    );
}

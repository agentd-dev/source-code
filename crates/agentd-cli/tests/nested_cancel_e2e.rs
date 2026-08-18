// SPDX-License-Identifier: AGPL-3.0-only
//! Nested-body **cancellation and deadlines** (RFC 0027 §7) end to end: when a
//! `foreach` element fails under `on_error: fail`, and when a `race` runs out
//! of time, the siblings still in flight must actually STOP — their steps
//! cancelled, their timers disarmed, their turn workers killed.
//!
//! The two regressions these tests exist for:
//!
//! 1. `cancel_scoped_children` matched children with `starts_with("{prefix}.")`,
//!    but an element/branch instance is keyed `parent[ix].step` /
//!    `parent{branch}.step` — neither starts with `parent.`. The race WINNER
//!    path passes an already-scoped id and worked; the three failure paths
//!    (`foreach` on_error fail, `parallel` on_error fail, race timeout) pass the
//!    parent's bare id and cancelled precisely nothing. A sibling's `sleep`
//!    stayed armed, so the instance outlived the run that abandoned it and the
//!    daemon — which counts a live timer as busy — could not idle-exit.
//! 2. `timeout` is a COMMON_FIELD: the parser lifts it into `step.timeout_ms`
//!    and never copies it into `spec`, so `race` read its deadline back out of
//!    `spec` and always got null. A `race` with a timeout waited forever.
//!
//! Both tests therefore assert that the process EXITS, promptly, under a hard
//! timeout: a regression fails fast instead of hanging CI for the full 90 s of
//! the sibling sleep it was supposed to cancel.

mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The sleep a cancelled sibling is parked on. Long enough that waiting it out
/// is unmistakably a regression rather than a slow machine.
const SIBLING_SLEEP: &str = "90s";

/// How long a bounded life may take before the test calls it wedged. The fixed
/// build exits in ~1.5 s (the failure plus `idle_grace`); the buggy one needs
/// 90 s, so anything in between separates them cleanly.
const HARD_TIMEOUT: Duration = Duration::from_secs(20);

fn write_file(tag: &str, ext: &str, body: &str) -> String {
    let path = common::unique_path(tag, ext);
    std::fs::write(&path, body).expect("write test file");
    path
}

/// A job-shaped v2 document with one inline workflow (JSON steps for brevity),
/// with no model and no MCP server — everything below is pure engine.
fn job(steps: &str) -> String {
    write_file(
        "agentd-nested-cancel",
        "yaml",
        &format!(
            "config_version: \"2\"\nagent:\n  name: nested-cancel\nworkflows:\n  - name: pipe\n    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n"
        ),
    )
}

/// One life of the daemon, killed (and failed) if it does not exit in time.
/// Unlike `Command::output()` this never blocks forever: an uncancelled
/// sibling must surface as a failing assertion, not a 90-second hang.
/// Returns `(exit code, stderr, how long it took)`.
fn run_agentd_bounded(config: &str) -> (Option<i32>, String, Duration) {
    let err_path = common::unique_path("agentd-nested-cancel", "err");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");
    let deadline = started + HARD_TIMEOUT;
    loop {
        match child.try_wait().expect("wait for agentd") {
            Some(status) => {
                let log = std::fs::read_to_string(&err_path).unwrap_or_default();
                let _ = std::fs::remove_file(&err_path);
                return (status.code(), log, started.elapsed());
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let log = std::fs::read_to_string(&err_path).unwrap_or_default();
                let _ = std::fs::remove_file(&err_path);
                panic!(
                    "agentd never exited (waited {HARD_TIMEOUT:?}): a nested body's siblings were never cancelled, so their {SIBLING_SLEEP} sleep timers keep the instance busy.\nstderr:\n{log}"
                );
            }
            None => std::thread::sleep(Duration::from_millis(25)),
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

/// Whether a scoped step reached `name` in the telemetry (`step.start` proves
/// it really ran; `step.done` proves it reached a terminal status of its own —
/// which a CANCELLED step never does, because cancellation rewrites the status
/// in place rather than finishing the step).
fn saw_step(stderr: &str, name: &str, step: &str) -> bool {
    events(stderr, name).iter().any(|e| e["step"] == step)
}

#[test]
fn a_failed_foreach_element_cancels_the_siblings_still_sleeping() {
    // Two elements at `parallel: 2`, so both are in flight at once. The body
    // routes on the item (a `switch`, not a `when` — this build has no CEL):
    // element 0 parks on a 90 s sleep, element 1 fails outright. `on_error`
    // defaults to `fail`, so the parent step fails and MUST take element 0's
    // sleeper down with it.
    let steps = format!(
        r#"{{
        "start": {{"kind": "once"}},
        "each": {{"kind": "foreach", "depends_on": ["start"], "over": ["hold", "boom"], "batch": {{"parallel": 2}},
                 "body": {{"steps": {{
                     "route": {{"kind": "switch", "on": "{{{{item}}}}", "cases": {{"hold": "sleeper", "boom": "explode"}}}},
                     "sleeper": {{"kind": "sleep", "depends_on": ["route"], "duration": "{SIBLING_SLEEP}"}},
                     "explode": {{"kind": "fail", "depends_on": ["route"], "message": "element {{{{index}}}} exploded"}}}}}}}},
        "done": {{"kind": "finish", "depends_on": ["each"], "status": "completed"}}
    }}"#
    );
    let (code, stderr, took) = run_agentd_bounded(&job(&steps));
    assert!(code.is_some(), "the daemon exited on its own:\n{stderr}");
    // The premise: element 0 really did start its long sleep, and element 1
    // really did fail — otherwise everything below holds vacuously.
    assert!(
        saw_step(&stderr, "step.start", "each[0].sleeper"),
        "the sibling element started sleeping:\n{stderr}"
    );
    assert!(
        events(&stderr, "step.done")
            .iter()
            .any(|e| e["step"] == "each[1].explode" && e["status"] == "failed"),
        "the second element failed:\n{stderr}"
    );
    // The regression: element 0's sleep was disarmed, so it never woke and the
    // instance went idle at once instead of outliving the run by 90 seconds.
    assert!(
        !saw_step(&stderr, "step.done", "each[0].sleeper"),
        "the cancelled sibling must never finish its sleep:\n{stderr}"
    );
    assert!(
        took < Duration::from_secs(15),
        "the instance idled out promptly (took {took:?}) — a still-armed sibling timer would hold it for {SIBLING_SLEEP}:\n{stderr}"
    );
    let done = events(&stderr, "run.done");
    assert_eq!(
        done.len(),
        1,
        "the run reached a terminal status:\n{stderr}"
    );
    assert_eq!(done[0]["status"], "failed", "{stderr}");
}

#[test]
fn a_race_timeout_fires_and_cancels_every_branch() {
    // Neither branch can ever win: the race's own `timeout` is the only thing
    // that can end this step, and when it fires both branches must be cancelled.
    let steps = format!(
        r#"{{
        "start": {{"kind": "once"}},
        "pick": {{"kind": "race", "depends_on": ["start"], "timeout": "300ms",
                 "branches": {{"slow": {{"steps": {{"s": {{"kind": "sleep", "duration": "{SIBLING_SLEEP}"}}}}}},
                              "slower": {{"steps": {{"s": {{"kind": "sleep", "duration": "{SIBLING_SLEEP}"}}}}}}}}}},
        "done": {{"kind": "finish", "depends_on": ["pick"], "status": "completed"}}
    }}"#
    );
    let (code, stderr, took) = run_agentd_bounded(&job(&steps));
    assert!(code.is_some(), "the daemon exited on its own:\n{stderr}");
    // The premise: both branches really were in flight when the deadline hit.
    for branch in ["pick{slow}.s", "pick{slower}.s"] {
        assert!(
            saw_step(&stderr, "step.start", branch),
            "branch step {branch} started:\n{stderr}"
        );
        assert!(
            !saw_step(&stderr, "step.done", branch),
            "branch step {branch} was cancelled, not slept out:\n{stderr}"
        );
    }
    // The deadline is honoured at all — it used to be read from a spec that
    // never carries it, leaving the race waiting on a 90 s branch.
    assert!(
        events(&stderr, "step.done")
            .iter()
            .any(|e| e["step"] == "pick" && e["status"] == "timeout"),
        "the race timed out:\n{stderr}"
    );
    assert!(
        took < Duration::from_secs(15),
        "the instance idled out promptly (took {took:?}):\n{stderr}"
    );
    let done = events(&stderr, "run.done");
    assert_eq!(
        done.len(),
        1,
        "the run reached a terminal status:\n{stderr}"
    );
    assert_eq!(done[0]["status"], "failed", "{stderr}");
}

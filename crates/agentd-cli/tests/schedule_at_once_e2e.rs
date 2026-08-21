// SPDX-License-Identifier: AGPL-3.0-only
//! Two scheduling invariants, end to end through the real binary and the file
//! store (so "durable" means a second process, not a second function call):
//!
//!   1. a `schedule` start with `at:` — a ONE-SHOT instant — fires **exactly
//!      once**. It used to re-arm itself on every tick past the instant
//!      (`next_schedule_ms` handed back `now + at` forever), so a workflow the
//!      operator asked to run once at 03:00 ran continuously from 03:00 on; and
//!      because the re-arm was recomputed at boot, a restart started it again.
//!   2. a run restored with a `Suspended` step whose durable timer is GONE is
//!      repaired at restore — re-armed (or failed) rather than left wedged.
//!      Nothing but `on_timer` wakes a `sleep`, so a lost timer used to mean a
//!      run that never moves again while the reactor ticks around it forever.
//!
//! Both tests are bounded: a wedged daemon must surface as a failing assertion,
//! never as a hung CI job.
#![cfg(feature = "workflow")]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long a bounded life may take before the test calls it wedged.
const HARD_TIMEOUT: Duration = Duration::from_secs(20);

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("schedule-at", "yaml");
    std::fs::write(&path, yaml).expect("write config");
    path
}

/// A per-test state root that does not exist yet.
fn state_root(tag: &str) -> String {
    let p = common::unique_path(tag, "state");
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Spawn a daemon with its stderr on disk (so the test can read it while the
/// process is still alive) — returns the child and the log path.
fn spawn_daemon(config: &str, state_dir: &str) -> (Child, String) {
    let err_path = common::unique_path("schedule-at", "err");
    let err = std::fs::File::create(&err_path).expect("create stderr file");
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .env("AGENTD_STATE_DIR", state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(err))
        .spawn()
        .expect("spawn agentd");
    (child, err_path)
}

/// Wait for `child` to exit, killing (and failing) it if it overruns.
fn wait_bounded(mut child: Child, err_path: &str, what: &str) -> (Option<i32>, String) {
    let deadline = Instant::now() + HARD_TIMEOUT;
    loop {
        match child.try_wait().expect("wait for agentd") {
            Some(status) => {
                let log = std::fs::read_to_string(err_path).unwrap_or_default();
                let _ = std::fs::remove_file(err_path);
                return (status.code(), log);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let log = std::fs::read_to_string(err_path).unwrap_or_default();
                panic!("{what}: agentd never exited (waited {HARD_TIMEOUT:?}).\nstderr:\n{log}");
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

/// Run a daemon until `marker` appears in its telemetry (bounded), HOLD it
/// alive `hold_ms` longer, then SIGTERM and collect. The hold is the point in
/// the one-shot test: the daemon must keep ticking past the outcome without
/// repeating it — while a fixed pre-outcome delay just races the CI runner's
/// load (observed: a 587 ms cold start ate a 900 ms budget and SIGTERM landed
/// between "start fired" and "run started").
fn run_daemon_until(
    config: &str,
    state_dir: &str,
    marker: &str,
    hold_ms: u64,
) -> (Option<i32>, String) {
    let (child, err_path) = spawn_daemon(config, state_dir);
    let pid = child.id() as i32;
    let deadline = Instant::now() + HARD_TIMEOUT;
    loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if !events(&log, marker).is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no {marker:?} within {HARD_TIMEOUT:?}:
{log}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    std::thread::sleep(Duration::from_millis(hold_ms));
    unsafe { libc::kill(pid, libc::SIGTERM) };
    wait_bounded(child, &err_path, "SIGTERMed life")
}

// ---------------------------------------------------------------------------
// 1. `at:` fires once, and stays fired.
// ---------------------------------------------------------------------------

/// `at` is a one-shot deadline (the implementation reads it as a delay from
/// arming): `0s` means "an instant that has already passed", the case the
/// operator hits at 03:00:00.001.
const AT_STEPS: &str = r#"{
    "at_three": {"kind": "schedule", "at": "0s"},
    "note": {"kind": "memory.set", "depends_on": ["at_three"], "key": "ran", "value": "1"},
    "done": {"kind": "finish", "depends_on": ["note"], "status": "completed"}
}"#;

fn at_config(dir: &str) -> String {
    write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: oneshot\nstore:\n  kind: file\n  file:\n    path: {dir}\n  checkpoint:\n    debounce_ms: 0\nworkflows:\n  - name: nightly\n    steps: {AT_STEPS}\nlifecycle:\n  run_until: drained\n  drain_timeout: 3s\nobservability:\n  log_level: info\n"
    ))
}

#[test]
fn a_schedule_with_at_fires_exactly_once_and_not_again_after_a_restart() {
    let dir = state_root("at-once");
    let cfg = at_config(&dir);

    // Life 1: the instant has passed, so the run fires and completes — and then
    // the daemon keeps ticking for the best part of a second. Every one of
    // those ticks used to fire it again.
    let (code, first) = run_daemon_until(&cfg, &dir, "run.done", 700);
    assert_eq!(code, Some(0), "life 1 drains; stderr:\n{first}");
    let fired = events(&first, "start.fired");
    let sched: Vec<&serde_json::Value> = fired.iter().filter(|e| e["kind"] == "schedule").collect();
    assert_eq!(
        sched.len(),
        1,
        "a one-shot `at` fires ONCE, not once per tick ({} firings):\n{first}",
        sched.len()
    );
    assert_eq!(
        events(&first, "run.start").len(),
        1,
        "and starts exactly one run:\n{first}"
    );
    let done = events(&first, "run.done");
    assert_eq!(done.len(), 1, "which completes:\n{first}");
    assert_eq!(done[0]["status"], "completed");

    // Life 2: the same state directory. "Already fired" is durable, so nothing
    // re-arms — an in-memory flag would have re-fired here. Wait for the
    // consumed one-shot to be REPORTED (the positive assertion below), then
    // hold: the negative assertions get their window without racing startup.
    let (code, second) = run_daemon_until(&cfg, &dir, "start.schedule.done", 500);
    assert_eq!(code, Some(0), "life 2 drains; stderr:\n{second}");
    assert!(
        events(&second, "restore.fresh").is_empty(),
        "life 2 restored the first life's state:\n{second}"
    );
    assert!(
        events(&second, "start.fired").is_empty(),
        "the one-shot `at` must NOT fire again after a restart:\n{second}"
    );
    assert!(
        events(&second, "run.start").is_empty(),
        "and no run starts:\n{second}"
    );
    assert_eq!(
        events(&second, "start.schedule.done").len(),
        1,
        "the consumed one-shot is reported as done, not as an invalid schedule:\n{second}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 2. A suspended step whose timer vanished is repaired at restore.
// ---------------------------------------------------------------------------

/// One `sleep` long enough to be suspended-and-durable when the process dies.
const SLEEP_STEPS: &str = r#"{
    "start": {"kind": "once"},
    "nap": {"kind": "sleep", "depends_on": ["start"], "duration": "1s"},
    "done": {"kind": "finish", "depends_on": ["nap"], "status": "completed"}
}"#;

fn sleep_config(dir: &str) -> String {
    write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: napper\nstore:\n  kind: file\n  file:\n    path: {dir}\n  checkpoint:\n    debounce_ms: 0\nworkflows:\n  - name: nap\n    steps: {SLEEP_STEPS}\nlifecycle:\n  run_until: idle\n  idle_grace: 500ms\nobservability:\n  log_level: info\n"
    ))
}

/// The file store's timer directory for this instance: `<root>/agentd/<name>/timer`.
fn timer_dir(dir: &str) -> std::path::PathBuf {
    std::path::Path::new(dir)
        .join("agentd")
        .join("napper")
        .join("timer")
}

/// Block until the sleep step has armed its durable timer (the row on disk is
/// the evidence that the step is suspended), then return its file paths.
fn wait_for_timer_rows(dir: &str) -> Vec<std::path::PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rows: Vec<std::path::PathBuf> = std::fs::read_dir(timer_dir(dir))
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        if !rows.is_empty() {
            return rows;
        }
        assert!(
            Instant::now() < deadline,
            "the sleep step never armed a durable timer under {}",
            timer_dir(dir).display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_suspended_step_whose_timer_is_gone_is_repaired_at_restore() {
    let dir = state_root("orphan-timer");
    let cfg = sleep_config(&dir);

    // Life 1: reach the suspended `sleep`, then die without warning.
    let (child, err_path) = spawn_daemon(&cfg, &dir);
    let pid = child.id() as i32;
    let rows = wait_for_timer_rows(&dir);
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let mut child = child;
    let _ = child.wait();
    let first = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    assert!(
        events(&first, "run.start").len() == 1,
        "life 1 started the run:\n{first}"
    );

    // The crash window this simulates: the timer row was deleted, the effect it
    // was carrying was not yet durable. What restore sees is a `Suspended` step
    // pointing at a timer that no longer exists.
    for row in &rows {
        std::fs::remove_file(row).expect("delete the timer row");
    }
    assert!(
        std::fs::read_dir(timer_dir(&dir))
            .map(|rd| rd.filter_map(Result::ok).count())
            .unwrap_or(0)
            == 0,
        "the timers are gone"
    );

    // Life 2: `run_until: idle` means a wedged step can never let the process
    // exit — so this call either proves the repair or times out loudly.
    let (child2, err2) = spawn_daemon(&cfg, &dir);
    let (code, second) = wait_bounded(child2, &err2, "life 2 (a step with no timer)");
    assert_eq!(code, Some(0), "life 2 exits cleanly; stderr:\n{second}");
    // Recovery, not a particular mechanism. Restore has two paths that can
    // rescue this step and they run in order: the generic step replay
    // (`restore.step.replay`, runtime/mod.rs) re-runs a step that was mid-flight
    // when the process died, and `repair_orphaned_timer_waits` re-arms a
    // `Suspended` step whose timer did not come back. Whichever engages first
    // legitimately leaves the other with nothing to do — asserting on one of
    // them fails whenever the other happens to win, which is exactly what a
    // repair test must not do. The property is that the step does not wedge.
    let repaired = events(&second, "restore.timer.repaired").len();
    let replayed = events(&second, "restore.step.replay").len();
    assert!(
        repaired + replayed >= 1,
        "restore rescued the step with no timer by one path or the other \
         (repaired={repaired}, replayed={replayed}):\n{second}"
    );
    let done = events(&second, "run.done");
    assert_eq!(
        done.len(),
        1,
        "the run finished rather than wedging:\n{second}"
    );
    assert_eq!(done[0]["status"], "completed", "{second}");
    let _ = std::fs::remove_dir_all(&dir);
}

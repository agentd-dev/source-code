// SPDX-License-Identifier: AGPL-3.0-only
//! **A drain lets running work finish, and parks what it must stop.**
//!
//! `SIGTERM` used to cancel every child at once and `SIGTERM` the process
//! group of any still running 5 seconds later. A step whose turn was in a
//! model call then failed with "worker exited without a result", its run
//! failed with it, and the next life had nothing to resume — the drain that
//! was meant to finish in-flight work destroyed it.
//!
//! Each test here drives a real daemon on the mock model, with a file store,
//! through a drain and, where it matters, a second life on the same state:
//!
//! - a step's turn that finishes inside `lifecycle.drain_timeout` records its
//!   result, the run parks at its next step, and the next life finishes the
//!   run without running the finished step again;
//! - a turn still running at the timeout — or when a second signal forces the
//!   drain — is stopped, the run is not failed, and the next life replays the
//!   step;
//! - a conversation turn the drain stops leaves its message in the inbox, and
//!   the next life answers it, given the message once;
//! - a subagent runs on through the drain the same way, and one the drain
//!   stops is `parked` and spawned again by the next life — a `detached` one
//!   too, and a `warm` one, which the drain stops as it begins; a `sync` one
//!   a stopped turn waited on is cancelled instead, since the replay asks for
//!   it again;
//! - an instance-tier child is sent `SIGTERM`, drains itself, is `parked`,
//!   and the next life spawns it again;
//! - a turn stopped at a gate leaves its task waiting, and the next life asks
//!   on it again;
//! - an exit asked for stops what runs but still runs the `lifecycle.shutdown`
//!   workflows, and a shutdown run a forced drain cuts short is cancelled, so
//!   the next life does not run it at its boot;
//! - a job the drain cut short does not exit 0;
//! - `agent_drains_total` says whether the drain completed or was forced.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long any single wait below may take before the test calls the daemon
/// wedged. Generous: the drains here take seconds, and these suites run beside
/// many others on a loaded host.
const WAIT: Duration = Duration::from_secs(90);

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The last lines of a log, for a failure message.
fn tail(log: &str) -> String {
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(60)..].join("\n")
}

/// Write the mock model's playbook: an `agent` step, a subagent or a
/// conversation turn asked for `STEP-ONE`, `SUB-TASK` or `ROOT-ASK` answers
/// after `slow_ms`; anything else at once.
/// The mock reads the file on every request, so a test rewrites it between
/// lives.
fn write_playbook(path: &str, slow_ms: u64) {
    std::fs::write(
        path,
        json!({"match": [
            {"when_contains": "STEP-TWO", "content": "two-done"},
            {"when_contains": "STEP-ONE", "delay_ms": slow_ms, "content": "one-done"},
            {"when_contains": "SUB-TASK", "delay_ms": slow_ms, "content": "sub-done"},
            {"when_contains": "ROOT-ASK", "delay_ms": slow_ms, "content": "root-done"}
        ]})
        .to_string(),
    )
    .unwrap();
}

/// A once-started run of two `agent` steps on the mock model, under `shape`
/// (`run_until`), with `drain` as `lifecycle.drain_timeout`.
fn two_steps(store: &str, playbook: &str, shape: &str, drain: &str, extra: &str) -> String {
    format!(
        "agent:\n  name: drainer\n\
         intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n\
         store:\n{store}\
         workflows:\n  - name: job\n    steps:\n\
         \x20     s:   {{kind: once}}\n\
         \x20     one: {{kind: agent, depends_on: [s], instruction: \"STEP-ONE\"}}\n\
         \x20     two: {{kind: agent, depends_on: [one], instruction: \"STEP-TWO\"}}\n\
         \x20     f:   {{kind: finish, depends_on: [two], status: completed, output: \"{{{{steps.two.output}}}}\"}}\n\
         lifecycle:\n  run_until: {shape}\n  drain_timeout: {drain}\n\
         observability:\n  log_level: info\n  log_content: true\n{extra}"
    )
}

/// A `file` store under `dir`, checkpointed without debounce.
fn file_store(dir: &str) -> String {
    format!("  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n")
}

/// One life of a daemon: its stderr in a file the test reads as it goes;
/// killed when the test ends, however it ends.
struct Life {
    child: Child,
    err_path: String,
}

impl Life {
    fn start(cfg: &str, env: &[(&str, &str)]) -> Life {
        let err_path = common::unique_path("drain-life", "log");
        let err = std::fs::File::create(&err_path).expect("stderr file");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
        cmd.args(["--config", cfg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(err));
        for (k, v) in env {
            cmd.env(k, v);
        }
        Life {
            child: cmd.spawn().expect("spawn agentd"),
            err_path,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.err_path).unwrap_or_default()
    }

    /// Wait until the log satisfies `want`, failing with `what` and the log.
    fn wait_for(&self, what: &str, want: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let log = self.log();
            if want(&log) {
                return log;
            }
            assert!(
                Instant::now() < deadline,
                "waited {WAIT:?} for {what}; daemon stderr (tail):\n{}",
                tail(&log)
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Wait until the step `step`'s turn worker is up — mid-call on the mock.
    fn wait_for_turn(&self, step: &str) {
        self.wait_for(&format!("step {step}'s turn"), |log| {
            events(log, "step.turn.spawn")
                .iter()
                .any(|e| e["step"] == step)
        });
    }

    fn signal(&self, sig: i32) {
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, sig) }, 0);
    }

    /// Wait for the process to exit; its code and its whole log.
    fn exit(&mut self) -> (Option<i32>, String) {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return (status.code(), self.log());
            }
            assert!(
                Instant::now() < deadline,
                "the daemon never exited; stderr (tail):\n{}",
                tail(&self.log())
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Life {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.err_path);
    }
}

/// No step failed and no run ended in this life.
fn nothing_failed(log: &str) {
    assert!(
        events(log, "step.done")
            .iter()
            .all(|e| e["status"] == "done"),
        "a step failed in the drain:\n{}",
        tail(log)
    );
    assert!(
        events(log, "run.done").is_empty(),
        "a run ended in the drain:\n{}",
        tail(log)
    );
    assert!(
        events(log, "turn.failed").is_empty(),
        "a turn failed in the drain:\n{}",
        tail(log)
    );
}

/// The second life on the same state: it finishes the run, then drains.
fn second_life(cfg: &str) -> String {
    let mut life = Life::start(cfg, &[]);
    life.wait_for("the run to complete in the second life", |log| {
        events(log, "run.done").iter().any(|e| {
            e["workflow"] == "job" && e["status"] == "completed" && e["output"] == "two-done"
        })
    });
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    log
}

#[test]
fn a_turn_that_finishes_inside_the_drain_is_recorded_and_its_run_resumes() {
    let dir = common::unique_path("drain-finish", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    // Longer than the 5 s the drain used to allow, well inside its timeout.
    write_playbook(&play, 8_000);
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        two_steps(&file_store(&dir), &play, "drained", "60s", ""),
    )
    .unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for_turn("one");
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    nothing_failed(&log);
    assert!(
        events(&log, "step.done")
            .iter()
            .any(|e| e["step"] == "one" && e["status"] == "done"),
        "the turn finished inside the drain and its step is done:\n{}",
        tail(&log)
    );
    assert!(
        !events(&log, "step.start")
            .iter()
            .any(|e| e["step"] == "two"),
        "no new step starts in a drain:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "drain.interrupted").is_empty(),
        "nothing was left for the next life:\n{}",
        tail(&log)
    );
    assert!(!events(&log, "drain.done").is_empty(), "{}", tail(&log));

    let log = second_life(&cfg);
    assert!(
        !events(&log, "step.start")
            .iter()
            .any(|e| e["step"] == "one"),
        "the finished step does not run again:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "restore.step.replay").is_empty(),
        "nothing was in flight to replay:\n{}",
        tail(&log)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// What a drain stopped is replayed: the step `one` it left `running` runs
/// again in the second life, and the run completes.
fn replayed(cfg: &str, play: &str, first: &str) {
    assert!(
        events(first, "drain.interrupted")
            .iter()
            .any(|e| e["steps"].as_array().is_some_and(|s| s
                .iter()
                .any(|s| s.as_str().is_some_and(|s| s.ends_with("/one"))))),
        "the drain names the step it left for the next life:\n{}",
        tail(first)
    );
    write_playbook(play, 0);
    let log = second_life(cfg);
    assert!(
        events(&log, "restore.step.replay")
            .iter()
            .any(|e| e["step"] == "one"),
        "the stopped step is replayed:\n{}",
        tail(&log)
    );
}

#[test]
fn a_turn_still_running_at_the_drain_timeout_is_stopped_not_failed() {
    let dir = common::unique_path("drain-timeout", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 120_000);
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        two_steps(&file_store(&dir), &play, "drained", "2s", ""),
    )
    .unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for_turn("one");
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "a daemon drains to 0:\n{}", tail(&log));
    nothing_failed(&log);
    replayed(&cfg, &play, &log);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_second_signal_forces_the_drain_and_stops_without_failing() {
    let dir = common::unique_path("drain-force", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 120_000);
    let cfg = format!("{dir}/c.yaml");
    // A timeout the test never reaches: only the second signal ends it.
    std::fs::write(
        &cfg,
        two_steps(&file_store(&dir), &play, "drained", "300s", ""),
    )
    .unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for_turn("one");
    life.signal(libc::SIGTERM);
    life.wait_for("the drain to begin", |log| {
        !events(log, "drain.start").is_empty()
    });
    let forced = Instant::now();
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert!(
        forced.elapsed() < Duration::from_secs(60),
        "the second signal ends the drain at once, not at its timeout"
    );
    assert_eq!(code, Some(0), "{}", tail(&log));
    nothing_failed(&log);
    replayed(&cfg, &play, &log);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A daemon whose once-started run is `steps`, beside a `sub` step.
fn subagent_config(dir: &str, playbook: &str, drain: &str, steps: &str) -> String {
    format!(
        "agent:\n  name: drainer\n\
         intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n\
         store:\n{store}\
         workflows:\n  - name: job\n    steps:\n\
         \x20     s:   {{kind: once}}\n{steps}\
         lifecycle:\n  run_until: drained\n  drain_timeout: {drain}\n\
         observability:\n  log_level: info\n  log_content: true\n",
        store = file_store(dir)
    )
}

/// A run that waits on an `async` subagent, spawned before the drain.
fn subagent_run(dir: &str, playbook: &str, drain: &str) -> String {
    subagent_config(
        dir,
        playbook,
        drain,
        "\x20     sub: {kind: subagent, depends_on: [s], mode: async, instruction: \"SUB-TASK\"}\n\
         \x20     got: {kind: wait, depends_on: [sub], on: subagent, subagent: \"{{steps.sub.output.handle}}\", timeout: 10m}\n\
         \x20     f:   {kind: finish, depends_on: [got], status: completed, output: \"{{steps.got.output.result}}\"}\n",
    )
}

/// A run that starts a subagent in `mode` and finishes without waiting on it.
fn subagent_left(dir: &str, playbook: &str, drain: &str, mode: &str) -> String {
    subagent_config(
        dir,
        playbook,
        drain,
        &format!(
            "\x20     sub: {{kind: subagent, depends_on: [s], mode: {mode}, instruction: \"SUB-TASK\"}}\n\
             \x20     f:   {{kind: finish, depends_on: [sub], status: completed}}\n"
        ),
    )
}

/// The handle of the one subagent a life spawned.
fn spawned_handle(log: &str) -> String {
    events(log, "subagent.spawn")
        .first()
        .and_then(|e| e["handle"].as_str())
        .unwrap_or_else(|| panic!("no subagent spawned:\n{}", tail(log)))
        .to_string()
}

/// The second life of a subagent run: it completes with the subagent's
/// answer, then drains.
fn subagent_second_life(cfg: &str) -> String {
    let mut life = Life::start(cfg, &[]);
    life.wait_for("the run to complete in the second life", |log| {
        events(log, "run.done").iter().any(|e| {
            e["workflow"] == "job" && e["status"] == "completed" && e["output"] == "sub-done"
        })
    });
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    log
}

#[test]
fn a_subagent_runs_on_through_the_drain_and_its_result_is_kept() {
    let dir = common::unique_path("drain-sub-finish", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 8_000);
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, subagent_run(&dir, &play, "60s")).unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for("the subagent", |log| {
        !events(log, "subagent.spawn").is_empty()
    });
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    let handle = spawned_handle(&log);
    assert!(
        events(&log, "subagent.result")
            .iter()
            .any(|e| e["handle"] == handle.as_str() && e["status"] == "completed"),
        "the subagent finished inside the drain:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "drain.interrupted").is_empty(),
        "{}",
        tail(&log)
    );

    let log = subagent_second_life(&cfg);
    assert!(
        events(&log, "subagent.respawn").is_empty(),
        "a finished subagent is not spawned again:\n{}",
        tail(&log)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_subagent_the_drain_stops_is_parked_and_comes_back() {
    let dir = common::unique_path("drain-sub-park", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 120_000);
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, subagent_run(&dir, &play, "2s")).unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for("the subagent", |log| {
        !events(log, "subagent.spawn").is_empty()
    });
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    let handle = spawned_handle(&log);
    assert!(
        events(&log, "subagent.result").is_empty(),
        "the stopped subagent is not recorded as failed:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "drain.interrupted")
            .iter()
            .any(|e| e["subagents"]
                .as_array()
                .is_some_and(|s| s.iter().any(|h| h == handle.as_str()))),
        "the drain names the subagent it parked:\n{}",
        tail(&log)
    );
    nothing_failed(&log);

    write_playbook(&play, 0);
    let log = subagent_second_life(&cfg);
    assert!(
        events(&log, "subagent.respawn")
            .iter()
            .any(|e| e["handle"] == handle.as_str()),
        "the parked subagent is spawned again:\n{}",
        tail(&log)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_detached_subagent_the_drain_stops_comes_back_too() {
    let dir = common::unique_path("drain-sub-detached", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 120_000);
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, subagent_left(&dir, &play, "2s", "detached")).unwrap();

    let mut life = Life::start(&cfg, &[]);
    // Nobody waits on it: its run is over before the drain begins.
    life.wait_for("the run that started the subagent", |log| {
        !events(log, "subagent.spawn").is_empty() && !events(log, "run.done").is_empty()
    });
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    let handle = spawned_handle(&log);
    assert!(events(&log, "subagent.result").is_empty(), "{}", tail(&log));

    // A restart after a crash does not revive a detached child; one the
    // drain stopped is not lost.
    write_playbook(&play, 0);
    let mut life = Life::start(&cfg, &[]);
    let log = life.wait_for("the parked subagent to finish", |log| {
        events(log, "subagent.result")
            .iter()
            .any(|e| e["handle"] == handle.as_str() && e["status"] == "completed")
    });
    assert!(
        events(&log, "subagent.respawn")
            .iter()
            .any(|e| e["handle"] == handle.as_str()),
        "{}",
        tail(&log)
    );
    life.signal(libc::SIGTERM);
    assert_eq!(life.exit().0, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_warm_subagent_is_stopped_as_the_drain_begins_and_comes_back() {
    let dir = common::unique_path("drain-sub-warm", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 0);
    let cfg = format!("{dir}/c.yaml");
    // A timeout the test never reaches: a warm subagent waits for its next
    // message for ever, so the drain must not wait it out.
    std::fs::write(&cfg, subagent_left(&dir, &play, "300s", "warm")).unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for("the warm subagent's first turn", |log| {
        !events(log, "subagent.turn").is_empty()
    });
    let began = Instant::now();
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert!(
        began.elapsed() < Duration::from_secs(60),
        "the drain stops a warm subagent rather than waiting out its timeout"
    );
    assert_eq!(code, Some(0), "{}", tail(&log));
    let handle = spawned_handle(&log);
    assert!(
        events(&log, "drain.interrupted")
            .iter()
            .any(|e| e["subagents"]
                .as_array()
                .is_some_and(|s| s.iter().any(|h| h == handle.as_str()))),
        "the drain names the warm subagent it parked:\n{}",
        tail(&log)
    );
    assert!(
        !events(&log, "subagent.result")
            .iter()
            .any(|e| e["handle"] == handle.as_str()),
        "the parked subagent's record is not closed:\n{}",
        tail(&log)
    );

    let mut life = Life::start(&cfg, &[]);
    let log = life.wait_for("the warm subagent to come back", |log| {
        events(log, "subagent.respawn")
            .iter()
            .any(|e| e["handle"] == handle.as_str())
    });
    drop(log);
    life.signal(libc::SIGTERM);
    assert_eq!(life.exit().0, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_conversation_turn_the_drain_stops_is_answered_by_the_next_life() {
    let dir = common::unique_path("drain-turn", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 120_000);
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: drainer\n  instruction: You answer questions.\n  preflight: never\n\
             intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n\
             store:\n{store}\
             workflows:\n  - name: idle\n    steps:\n      s: {{kind: manual}}\n      f: {{kind: finish, depends_on: [s]}}\n\
             lifecycle:\n  run_until: drained\n  drain_timeout: 2s\n\
             observability:\n  log_level: info\n  log_content: true\n",
            store = file_store(&dir)
        ),
    )
    .unwrap();
    let inbox = format!("{dir}/inbox.json");
    std::fs::write(
        &inbox,
        json!([{"kind": "a2a_message", "principal": "user:alice",
                "payload": {"context_id": "c1", "text": "ROOT-ASK what changed?"}}])
        .to_string(),
    )
    .unwrap();

    let mut life = Life::start(&cfg, &[("AGENTD_TEST_INBOX_FILE", &inbox)]);
    let log = life.wait_for("the conversation turn", |log| {
        !events(log, "turn.spawn").is_empty()
    });
    let event = events(&log, "turn.spawn")[0]["inbox_event"].clone();
    assert!(event.is_string(), "{}", tail(&log));
    // What the turn was given: its worker says how many messages.
    let log = life.wait_for("the turn to start", |log| {
        !events(log, "turn.start").is_empty()
    });
    let given = events(&log, "turn.start")[0]["messages"].clone();
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    assert!(
        events(&log, "turn.failed").is_empty() && events(&log, "turn.reply").is_empty(),
        "the stopped turn is neither failed nor answered:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "drain.interrupted").iter().any(|e| e["turns"]
            .as_array()
            .is_some_and(|t| t.iter().any(|t| t["inbox_event"] == event))),
        "the drain names the turn it left:\n{}",
        tail(&log)
    );

    // The message is still in the inbox: the next life answers it.
    write_playbook(&play, 0);
    let mut life = Life::start(&cfg, &[]);
    let log = life.wait_for("the replayed turn's reply", |log| {
        events(log, "turn.reply")
            .iter()
            .any(|e| e["ctx"] == "c1" && e["text"] == "root-done")
    });
    assert!(
        events(&log, "turn.spawn")
            .iter()
            .any(|e| e["inbox_event"] == event),
        "the same inbox event is answered:\n{}",
        tail(&log)
    );
    assert_eq!(
        events(&log, "turn.start")[0]["messages"],
        given,
        "the replayed turn is given the message once, not twice:\n{}",
        tail(&log)
    );
    life.signal(libc::SIGTERM);
    assert_eq!(life.exit().0, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_job_the_drain_cuts_short_does_not_exit_0() {
    let dir = common::unique_path("drain-job", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 120_000);
    let cfg = format!("{dir}/c.yaml");
    // A one-shot job with nowhere to keep what it did not finish.
    std::fs::write(&cfg, two_steps("  kind: memory\n", &play, "idle", "2s", "")).unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for_turn("one");
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(
        code,
        Some(3),
        "a job whose run the drain left unfinished exits 3, never 0:\n{}",
        tail(&log)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `/metrics` read off a daemon on `cfg_text` (which serves them), held at
/// the end of its drain (`AGENTD_TEST_HOLD_AT`) so the scrape sees the
/// outcome before the process exits. `begin` starts the drain.
#[cfg(feature = "metrics")]
fn scrape_at_drain_end(dir: &str, cfg_text: &str, begin: impl FnOnce(&Life)) -> String {
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let release = format!("{dir}/release");
    let mut life = Life::start(
        &cfg,
        &[
            ("AGENTD_TEST_HOLD_AT", "drain.done"),
            ("AGENTD_TEST_HOLD_RELEASE", &release),
        ],
    );
    let log = life.wait_for("metrics.serving", |log| {
        !events(log, "metrics.serving").is_empty()
    });
    let addr = events(&log, "metrics.serving")[0]["addr"]
        .as_str()
        .expect("addr")
        .to_string();
    begin(&life);
    life.wait_for("the drain to end", |log| {
        !events(log, "drain.done").is_empty()
    });
    let scrape = common::http_get(&addr, "/metrics").body;
    std::fs::write(&release, "").unwrap();
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    scrape
}

/// [`scrape_at_drain_end`] for a `SIGTERM` while step `one`'s turn takes
/// `slow_ms`, under the timeout `drain`.
#[cfg(feature = "metrics")]
fn drains_counted(slow_ms: u64, drain: &str) -> String {
    let dir = common::unique_path("drain-metrics", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, slow_ms);
    let cfg = two_steps(
        &file_store(&dir),
        &play,
        "drained",
        drain,
        "  metrics_addr: \"127.0.0.1:0\"\n",
    );
    let scrape = scrape_at_drain_end(&dir, &cfg, |life| {
        life.wait_for_turn("one");
        life.signal(libc::SIGTERM);
    });
    let _ = std::fs::remove_dir_all(&dir);
    scrape
}

/// [`scrape_at_drain_end`] for a daemon whose conversation turn asks to exit
/// with nothing else running, while a `lifecycle.shutdown` workflow
/// deregisters: the drain runs it and stops nothing.
#[cfg(feature = "metrics")]
fn exit_counted() -> String {
    let (port, calls) = spawn_recorder();
    let dir = common::unique_path("drain-metrics-exit", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    std::fs::write(
        &play,
        json!({"match": [{"when_contains": "FINISH-NOW", "tool_calls": [{"name": "finish",
            "arguments": {"status": "completed", "exit": true}}]}]})
        .to_string(),
    )
    .unwrap();
    let cfg = format!(
        "agent:\n  name: drainer\n  instruction: You run jobs.\n  preflight: never\n\
         intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n\
         store:\n{store}\
         workflows:\n  - name: stopper\n    steps:\n\
         \x20     s:    {{kind: once}}\n\
         \x20     poke: {{kind: message, depends_on: [s], to: root, text: \"FINISH-NOW\"}}\n\
         \x20     f:    {{kind: finish, depends_on: [poke]}}\n\
         \x20 - name: deinit\n    steps:\n\
         \x20     bye:        {{kind: event, on: lifecycle.shutdown}}\n\
         \x20     deregister: {{kind: http, depends_on: [bye], method: DELETE, url: \"http://127.0.0.1:{port}/webhooks\", allow_private: true}}\n\
         \x20     f:          {{kind: finish, depends_on: [deregister], status: completed}}\n\
         lifecycle:\n  run_until: drained\n  drain_timeout: 60s\n\
         observability:\n  log_level: info\n  metrics_addr: \"127.0.0.1:0\"\n",
        store = file_store(&dir)
    );
    let scrape = scrape_at_drain_end(&dir, &cfg, |_| {});
    assert_eq!(deregistrations(&calls), 1);
    let _ = std::fs::remove_dir_all(&dir);
    scrape
}

#[cfg(feature = "metrics")]
#[test]
fn a_drain_counts_completed_or_forced() {
    let sample = |scrape: &str, phase: &str| {
        let series = format!("agent_drains_total{{phase=\"{phase}\"}} ");
        scrape
            .lines()
            .find_map(|l| l.strip_prefix(series.as_str())?.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("no {series}sample:\n{scrape}"))
    };
    let clean = drains_counted(3_000, "60s");
    assert_eq!(sample(&clean, "started"), 1, "{clean}");
    assert_eq!(sample(&clean, "completed"), 1, "{clean}");
    assert_eq!(sample(&clean, "forced"), 0, "{clean}");
    let cut = drains_counted(120_000, "2s");
    assert_eq!(sample(&cut, "started"), 1, "{cut}");
    assert_eq!(sample(&cut, "forced"), 1, "{cut}");
    assert_eq!(sample(&cut, "completed"), 0, "{cut}");
    // An exit that waits for its shutdown workflow cut nothing short.
    let exit = exit_counted();
    assert_eq!(sample(&exit, "started"), 1, "{exit}");
    assert_eq!(sample(&exit, "completed"), 1, "{exit}");
    assert_eq!(sample(&exit, "forced"), 0, "{exit}");
}

/// A run whose step `one` waits on a `sync` subagent its turn starts: the
/// subagent answers after `slow_ms`, and the step's turn with what it said.
fn write_sync_playbook(path: &str, slow_ms: u64) {
    std::fs::write(
        path,
        json!({"match": [
            {"when_contains": "sub-done", "content": "one-done"},
            {"when_contains": "SUB-TASK", "delay_ms": slow_ms, "content": "sub-done"},
            {"when_contains": "STEP-ONE", "tool_calls": [{"name": "subagent.run",
                "arguments": {"instruction": "SUB-TASK", "mode": "sync"}}]},
            {"when_contains": "STEP-TWO", "content": "two-done"}
        ]})
        .to_string(),
    )
    .unwrap();
}

#[test]
fn a_sync_subagent_a_stopped_turn_waited_on_is_cancelled_not_run_twice() {
    let dir = common::unique_path("drain-sub-sync", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_sync_playbook(&play, 120_000);
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        two_steps(&file_store(&dir), &play, "drained", "2s", ""),
    )
    .unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for("the sync subagent", |log| {
        !events(log, "subagent.spawn").is_empty()
    });
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    nothing_failed(&log);
    let handle = spawned_handle(&log);
    assert!(
        events(&log, "subagent.result")
            .iter()
            .any(|e| e["handle"] == handle.as_str() && e["status"] == "cancelled"),
        "the subagent the stopped turn waited on is cancelled:\n{}",
        tail(&log)
    );
    assert!(
        !events(&log, "drain.interrupted")
            .iter()
            .any(|e| e["subagents"]
                .as_array()
                .is_some_and(|s| s.iter().any(|h| h == handle.as_str()))),
        "it is not left for the next life:\n{}",
        tail(&log)
    );

    // The step's replay asks for a subagent again; the old one stays down.
    write_sync_playbook(&play, 0);
    let log = second_life(&cfg);
    assert!(
        events(&log, "restore.step.replay")
            .iter()
            .any(|e| e["step"] == "one"),
        "{}",
        tail(&log)
    );
    assert!(
        events(&log, "subagent.respawn").is_empty(),
        "the cancelled subagent is not spawned again beside the replay's:\n{}",
        tail(&log)
    );
    let spawned = events(&log, "subagent.spawn");
    assert_eq!(spawned.len(), 1, "{}", tail(&log));
    assert_ne!(spawned[0]["handle"], handle.as_str(), "{}", tail(&log));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stand-in third party: records each request line it is sent, and answers
/// every one `200 {"ok":true}`.
fn spawn_recorder() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = calls.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut reader = BufReader::new(stream);
            let mut start = String::new();
            if reader.read_line(&mut start).is_err() || start.is_empty() {
                continue;
            }
            let mut len = 0usize;
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).is_err() || l.trim().is_empty() {
                    break;
                }
                if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).ok();
            seen.lock().unwrap().push(start.trim().to_string());
            let mut s = reader.into_inner();
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
            let _ = s.flush();
        }
    });
    (port, calls)
}

/// How many `DELETE /webhooks` the recorder was sent.
fn deregistrations(calls: &std::sync::Mutex<Vec<String>>) -> usize {
    calls
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("DELETE /webhooks"))
        .count()
}

/// The two-step `job` beside a deinit workflow that deregisters from the
/// recorder on `port`, after its `hold` steps, and `extra` workflows.
fn deinit_config(
    dir: &str,
    playbook: &str,
    port: u16,
    drain: &str,
    hold: &str,
    extra: &str,
) -> String {
    format!(
        "agent:\n  name: drainer\n  instruction: You run jobs.\n  preflight: never\n\
         intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n\
         store:\n{store}\
         workflows:\n  - name: job\n    steps:\n\
         \x20     s:   {{kind: once}}\n\
         \x20     one: {{kind: agent, depends_on: [s], instruction: \"STEP-ONE\"}}\n\
         \x20     two: {{kind: agent, depends_on: [one], instruction: \"STEP-TWO\"}}\n\
         \x20     f:   {{kind: finish, depends_on: [two], status: completed, output: \"{{{{steps.two.output}}}}\"}}\n\
         \x20 - name: deinit\n    steps:\n\
         \x20     bye:        {{kind: event, on: lifecycle.shutdown}}\n{hold}\
         \x20     deregister: {{kind: http, depends_on: [{last}], method: DELETE, url: \"http://127.0.0.1:{port}/webhooks\", allow_private: true}}\n\
         \x20     f:          {{kind: finish, depends_on: [deregister], status: completed}}\n{extra}\
         lifecycle:\n  run_until: drained\n  drain_timeout: {drain}\n\
         observability:\n  log_level: info\n  log_content: true\n",
        store = file_store(dir),
        last = if hold.is_empty() { "bye" } else { "hold" },
    )
}

#[test]
fn an_exit_asked_for_stops_what_runs_and_still_runs_the_shutdown_workflows() {
    let (port, calls) = spawn_recorder();
    let dir = common::unique_path("drain-exit", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    let write = |slow_ms: u64| {
        std::fs::write(
            &play,
            json!({"match": [
                {"when_contains": "FINISH-NOW", "tool_calls": [{"name": "finish",
                    "arguments": {"status": "completed", "exit": true}}]},
                {"when_contains": "STEP-TWO", "content": "two-done"},
                {"when_contains": "STEP-ONE", "delay_ms": slow_ms, "content": "one-done"}
            ]})
            .to_string(),
        )
        .unwrap();
    };
    write(120_000);
    let cfg = format!("{dir}/c.yaml");
    // Once step `one`'s turn is in its call, a conversation turn asks to exit.
    // The timeout is never reached: the exit stops `one` at once.
    let stopper = "\x20 - name: stopper\n    steps:\n\
         \x20     s:    {kind: once}\n\
         \x20     nap:  {kind: sleep, depends_on: [s], duration: 2s}\n\
         \x20     poke: {kind: message, depends_on: [nap], to: root, text: \"FINISH-NOW\"}\n\
         \x20     f:    {kind: finish, depends_on: [poke]}\n";
    std::fs::write(&cfg, deinit_config(&dir, &play, port, "300s", "", stopper)).unwrap();

    let began = Instant::now();
    let mut life = Life::start(&cfg, &[]);
    life.wait_for_turn("one");
    let (code, log) = life.exit();
    assert_eq!(
        code,
        Some(0),
        "finish {{exit: true}} exits 0:\n{}",
        tail(&log)
    );
    assert!(
        began.elapsed() < Duration::from_secs(120),
        "the exit stops the running turn rather than waiting it out"
    );
    assert!(
        events(&log, "root.finish")
            .iter()
            .any(|e| e["exit"] == true),
        "{}",
        tail(&log)
    );
    assert!(
        events(&log, "run.done")
            .iter()
            .any(|e| e["workflow"] == "deinit" && e["status"] == "completed"),
        "the shutdown workflow ran before the exit:\n{}",
        tail(&log)
    );
    assert_eq!(deregistrations(&calls), 1, "{}", tail(&log));
    assert!(
        !events(&log, "run.done")
            .iter()
            .any(|e| e["workflow"] == "job" || e["status"] != "completed"),
        "the stopped run is not ended, and no run failed:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "step.done")
            .iter()
            .all(|e| e["status"] == "done"),
        "{}",
        tail(&log)
    );
    replayed(&cfg, &play, &log);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_shutdown_run_a_forced_drain_cuts_short_is_cancelled_not_run_next_life() {
    let (port, calls) = spawn_recorder();
    let dir = common::unique_path("drain-deinit-cut", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    let write = |slow_ms: u64| {
        std::fs::write(
            &play,
            json!({"match": [
                {"when_contains": "DEINIT-HOLD", "delay_ms": slow_ms, "content": "held"},
                {"when_contains": "STEP-TWO", "content": "two-done"},
                {"when_contains": "STEP-ONE", "delay_ms": slow_ms, "content": "one-done"}
            ]})
            .to_string(),
        )
        .unwrap();
    };
    write(120_000);
    let cfg = format!("{dir}/c.yaml");
    let hold =
        "\x20     hold:       {kind: agent, depends_on: [bye], instruction: \"DEINIT-HOLD\"}\n";
    std::fs::write(&cfg, deinit_config(&dir, &play, port, "2s", hold, "")).unwrap();

    let mut life = Life::start(&cfg, &[]);
    life.wait_for_turn("one");
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    let deinit: Vec<Value> = events(&log, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "deinit")
        .collect();
    assert!(
        deinit.len() == 1 && deinit[0]["status"] == "cancelled",
        "the cut shutdown run is cancelled:\n{}",
        tail(&log)
    );
    let cut = deinit[0]["run"].clone();
    assert!(
        events(&log, "drain.interrupted").iter().any(|e| e["runs"]
            .as_array()
            .is_some_and(|r| r.contains(&cut))
            && !e["steps"].as_array().is_some_and(|s| s
                .iter()
                .any(|s| s.as_str().is_some_and(|s| s.ends_with("/hold"))))),
        "the drain names the shutdown run it cut, not its step:\n{}",
        tail(&log)
    );
    assert_eq!(deregistrations(&calls), 0, "{}", tail(&log));

    // The next life finishes the job, and does not run the last one's
    // shutdown at its boot.
    write(0);
    let mut life = Life::start(&cfg, &[]);
    let log = life.wait_for("the run to complete in the second life", |log| {
        events(log, "run.done").iter().any(|e| {
            e["workflow"] == "job" && e["status"] == "completed" && e["output"] == "two-done"
        })
    });
    assert!(
        !events(&log, "restore.step.replay")
            .iter()
            .any(|e| e["step"] == "hold"),
        "the cancelled shutdown run is not replayed:\n{}",
        tail(&log)
    );
    assert_eq!(
        deregistrations(&calls),
        0,
        "nothing deregisters before this life's drain:\n{}",
        tail(&log)
    );
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    assert_eq!(deregistrations(&calls), 1, "{}", tail(&log));
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(feature = "a2a")]
#[test]
fn an_instance_child_drains_with_its_parent_and_comes_back() {
    let dir = common::unique_path("drain-instance", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        format!(
            "agent: {{ name: parent }}\n\
             store:\n{store}\
             lifecycle: {{ run_until: drained, drain_timeout: 60s }}\n\
             observability: {{ log_level: info, log_content: true }}\n\
             subagents:\n\
            \x20 templates:\n\
            \x20   room:\n\
            \x20     instruction: |\n\
            \x20       You are the room for {{{{params.id}}}}.\n\
            \x20       :::!workflow{{name=on-ping}}\n\
            \x20       steps:\n\
            \x20         cmd: {{ kind: a2a, command: room.ping, roles: [agent, operator] }}\n\
            \x20         f:   {{ kind: finish, depends_on: [cmd], status: completed, output: pong }}\n\
            \x20       :::\n\
            \x20     params: {{ id: {{ type: string, required: true }} }}\n\
             workflows:\n  - name: caller\n    steps:\n\
            \x20     s:     {{ kind: once }}\n\
            \x20     spawn: {{ kind: subagent, template: room, params: {{ id: inc-7 }}, depends_on: [s] }}\n\
            \x20     f:     {{ kind: finish, depends_on: [spawn], status: completed }}\n",
            store = file_store(&dir)
        ),
    )
    .unwrap();

    let mut life = Life::start(&cfg, &[]);
    let log = life.wait_for("the instance child to be up", |log| {
        events(log, "step.done")
            .iter()
            .any(|e| e["step"] == "spawn" && e["status"] == "done")
    });
    let spawned = events(&log, "instance.spawn");
    let handle = spawned[0]["handle"].as_str().expect("handle").to_string();
    let pid = spawned[0]["pid"].as_i64().expect("pid") as i32;
    let began = Instant::now();
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    assert!(
        began.elapsed() < Duration::from_secs(45),
        "the child drains itself; the parent does not wait out its timeout"
    );
    assert!(
        events(&log, "instance.drain")
            .iter()
            .any(|e| e["handle"] == handle.as_str()),
        "the drain asks the child to drain itself:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "instance.exited")
            .iter()
            .any(|e| e["handle"] == handle.as_str() && e["status"] == "parked"),
        "the child that drained is parked, not closed:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "drain.interrupted")
            .iter()
            .any(|e| e["subagents"]
                .as_array()
                .is_some_and(|s| s.iter().any(|h| h == handle.as_str()))),
        "{}",
        tail(&log)
    );
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the parent waited for its child to exit"
    );

    let mut life = Life::start(&cfg, &[]);
    life.wait_for("the instance child to come back", |log| {
        events(log, "instance.respawn")
            .iter()
            .any(|e| e["handle"] == handle.as_str())
    });
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    assert!(
        events(&log, "instance.exited")
            .iter()
            .any(|e| e["handle"] == handle.as_str() && e["status"] == "parked"),
        "{}",
        tail(&log)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A daemon on the A2A listener `port` whose conversation turn asks a person
/// before it answers.
#[cfg(feature = "a2a")]
fn gate_config(dir: &str, playbook: &str, port: u16) -> String {
    format!(
        "agent:\n  name: drainer\n  instruction: You roll things out.\n  preflight: never\n\
         intelligence:\n  endpoints: \"mock:file:{playbook}\"\n  model: mock\n\
         store:\n{store}\
         a2a:\n  listen: http://127.0.0.1:{port}\n\
         lifecycle:\n  run_until: drained\n  drain_timeout: 2s\n\
         observability:\n  log_level: info\n  log_content: true\n",
        store = file_store(dir)
    )
}

/// A life of [`gate_config`] on a port it bound; its listener's authority.
#[cfg(feature = "a2a")]
fn gate_life(dir: &str, playbook: &str) -> (Life, String) {
    let cfg = format!("{dir}/c.yaml");
    for _ in 0..5 {
        std::fs::write(&cfg, gate_config(dir, playbook, common::free_port())).unwrap();
        let life = Life::start(&cfg, &[]);
        if let Some(addr) = common::try_a2a_bound(&life.err_path, Duration::from_secs(20)) {
            return (life, addr);
        }
    }
    panic!("the daemon never bound an A2A listener (5 attempts)");
}

/// Poll `GetTask` until `want` holds, failing early if the log shows `stop`.
#[cfg(feature = "a2a")]
fn wait_task(
    life: &Life,
    addr: &str,
    id: &str,
    what: &str,
    want: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let t = common::rpc_result(addr, 7, "GetTask", json!({"id": id}));
        if want(&t) {
            return t;
        }
        assert!(
            Instant::now() < deadline,
            "waited {WAIT:?} for {what}; last: {t}\n{}",
            tail(&life.log())
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(feature = "a2a")]
#[test]
fn a_turn_stopped_at_a_gate_leaves_its_task_waiting_and_asks_again() {
    let dir = common::unique_path("drain-gate", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    std::fs::write(
        &play,
        json!({"turns": [
            {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Which region first?"}}]},
            {"content": "Rolled out."}
        ]})
        .to_string(),
    )
    .unwrap();
    let input_required = |t: &Value| t["status"]["state"] == "TASK_STATE_INPUT_REQUIRED";

    let (mut life, addr) = gate_life(&dir, &play);
    let sent = common::SendMessage::text("Roll it out")
        .return_immediately()
        .result(&addr);
    let task = sent["task"]["id"].as_str().expect("task id").to_string();
    wait_task(&life, &addr, &task, "the gate", input_required);
    life.signal(libc::SIGTERM);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    assert!(
        events(&log, "human.ask.pruned").is_empty(),
        "the stopped turn's gate is not failed:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "human.ask.drained")
            .iter()
            .any(|e| e["task"] == task.as_str() && e["state"] == "TASK_STATE_WORKING"),
        "the gate's task goes back to working:\n{}",
        tail(&log)
    );
    assert!(
        events(&log, "drain.interrupted")
            .iter()
            .any(|e| e["turns"].as_array().is_some_and(|t| !t.is_empty())),
        "{}",
        tail(&log)
    );

    // The next life replays the turn, which asks on the same task again.
    let (mut life, addr) = gate_life(&dir, &play);
    wait_task(&life, &addr, &task, "the gate asked again", |t| {
        assert!(
            !matches!(
                t["status"]["state"].as_str(),
                Some("TASK_STATE_FAILED" | "TASK_STATE_COMPLETED")
            ),
            "the task ended without its gate: {t}\n{}",
            tail(&life.log())
        );
        input_required(t)
    });
    assert!(
        events(&life.log(), "human.ask")
            .iter()
            .any(|e| e["task"] == task.as_str()),
        "{}",
        tail(&life.log())
    );
    common::SendMessage::text("eu-west")
        .task(&task)
        .return_immediately()
        .result(&addr);
    let done = wait_task(&life, &addr, &task, "the answered turn", |t| {
        t["status"]["state"] == "TASK_STATE_COMPLETED"
    });
    assert!(done.to_string().contains("Rolled out."), "{done}");
    life.signal(libc::SIGTERM);
    assert_eq!(life.exit().0, Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_drain_a_step_begins_starts_none_of_the_steps_ready_beside_it() {
    let dir = common::unique_path("drain-lifetime", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let play = format!("{dir}/play.json");
    write_playbook(&play, 0);
    let cfg = format!("{dir}/c.yaml");
    // Two steps ready at once. The first one admitted spends the lifetime
    // ceiling, which begins the drain; its failure does not end the run, and
    // the second must not start after it.
    std::fs::write(
        &cfg,
        format!(
            "agent:\n  name: drainer\n\
             intelligence:\n  endpoints: \"mock:file:{play}\"\n  model: mock\n\
             \x20 budget:\n    lifetime_tokens: 1\n    lifetime_exhausted: drain\n\
             store:\n{store}\
             workflows:\n  - name: job\n    steps:\n\
             \x20     s: {{kind: once}}\n\
             \x20     a: {{kind: agent, depends_on: [s], instruction: \"STEP-ONE\", on_error: continue}}\n\
             \x20     b: {{kind: agent, depends_on: [s], instruction: \"STEP-TWO\", on_error: continue}}\n\
             \x20     f: {{kind: finish, depends_on: [a, b], status: completed}}\n\
             lifecycle:\n  run_until: drained\n  drain_timeout: 60s\n\
             observability:\n  log_level: info\n",
            store = file_store(&dir)
        ),
    )
    .unwrap();

    let mut life = Life::start(&cfg, &[]);
    let (code, log) = life.exit();
    assert_eq!(code, Some(0), "{}", tail(&log));
    assert!(
        !events(&log, "budget.lifetime_exhausted").is_empty(),
        "{}",
        tail(&log)
    );
    let started: Vec<Value> = events(&log, "step.start")
        .into_iter()
        .filter(|e| e["step"] == "a" || e["step"] == "b")
        .collect();
    assert_eq!(
        started.len(),
        1,
        "only the step that spent the ceiling started:\n{}",
        tail(&log)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

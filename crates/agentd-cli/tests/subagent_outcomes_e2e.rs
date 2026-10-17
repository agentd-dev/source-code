// SPDX-License-Identifier: AGPL-3.0-only
//! A subagent's record ends with the child, however the child ends: on one
//! of its budgets, or killed without a word. A record left looking live held
//! a `limits.subagents.breadth` slot for ever, never answered a wait on it,
//! and was spawned again at the next start.
//!
//! A `subagent.kill` ends the child within seconds, wherever it is, and
//! leaves its record `cancelled` with the kill's reason, whatever the child
//! does after. A child between steps stops on the cancel; one inside a model
//! call, which cannot read the cancel until the call returns, is sent
//! `SIGTERM` at the grace.
//! The kill used to stop at the cancel: a child in a long call ran on, and
//! spent, until the call returned, and a result it reported then completed
//! the record it had been killed in.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn playbook(body: Value) -> String {
    let path = common::unique_path("outcomes-play", "json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A job that spawns one async child and waits on it for up to 30 s.
fn config(playbook: &str, limits: &str) -> String {
    let path = common::unique_path("outcomes", "yaml");
    std::fs::write(
        &path,
        format!(
            "agent: {{ name: o }}\nstore: {{ kind: memory }}\nobservability: {{ log_level: info }}\n\
             intelligence: {{ endpoints: \"mock:file:{playbook}\", model: m }}\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     sub: {{ kind: subagent, depends_on: [s], mode: async, instruction: \"Keep going.\"{limits} }}\n\
             \x20     w: {{ kind: wait, depends_on: [sub], on: subagent, subagent: \"{{{{steps.sub.output.handle}}}}\", timeout: 30s }}\n\
             \x20     f: {{ kind: finish, depends_on: [w], status: completed }}\n"
        ),
    )
    .unwrap();
    path
}

/// A child stopped by its step budget ends its record `exhausted_steps`, and
/// the wait on it resolves at once rather than at its timeout.
#[test]
fn a_child_stopped_by_its_budget_answers_the_wait_on_it() {
    // Every round calls a tool, so the child never finishes on its own.
    let play = playbook(json!({"turns": [{"tool_calls": [{"name": "spin", "arguments": {}}]}]}));
    let cfg = config(&play, ", limits: { steps: 1 }");
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    let result = events(&log, "subagent.result");
    assert_eq!(result.len(), 1, "{log}");
    assert_eq!(result[0]["status"], "exhausted_steps", "{log}");
    assert!(
        !log.contains("wait timed out") && started.elapsed() < Duration::from_secs(20),
        "the wait waited out its timeout: {:?}\n{log}",
        started.elapsed()
    );
    // The wait answered with the child's end, which is not a completion.
    assert!(
        events(&log, "step.done")
            .iter()
            .any(|e| e["step"] == "w" && e["status"] == "failed"),
        "{log}"
    );
    assert_eq!(events(&log, "run.done")[0]["status"], "failed", "{log}");
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
}

/// A child killed without a result is failed, and the wait on it resolves.
#[test]
fn a_child_killed_without_a_result_fails_and_answers_the_wait_on_it() {
    // The child sits in its first model call long enough to be killed there.
    let play = playbook(json!({"turns": [{"content": "late", "delay_ms": 20000}]}));
    let cfg = config(&play, "");
    let err_path = common::unique_path("outcomes", "log");
    let started = Instant::now();
    let mut agentd = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn");
    let log = || std::fs::read_to_string(&err_path).unwrap_or_default();
    let pid = loop {
        if let Some(pid) = events(&log(), "subagent.spawn")
            .first()
            .and_then(|e| e["pid"].as_i64())
        {
            break pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "no child: {}",
            log()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let status = loop {
        if let Some(s) = agentd.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(25) {
            let _ = agentd.kill();
            let _ = agentd.wait();
            panic!("the wait was never answered:\n{}", log());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let log = log();
    let result = events(&log, "subagent.result");
    assert_eq!(result.len(), 1, "{log}");
    assert_eq!(result[0]["status"], "failed", "{log}");
    assert!(
        result[0]["err"]
            .as_str()
            .is_some_and(|e| e.contains("exited without a result")),
        "{log}"
    );
    assert!(!status.success(), "the run failed with its child: {log}");
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
    let _ = std::fs::remove_file(&err_path);
}

/// A child the supervisor tears down — here stuck: stopped, so it reports
/// nothing and answers no ping — is settled as an exit: its record fails and
/// the wait on it resolves. The kill reaps the child itself, so the reaper
/// never reported the exit; the record stayed `running` and the child's
/// entry stayed in the table.
#[test]
fn a_child_the_supervisor_kills_fails_and_answers_the_wait_on_it() {
    let play = playbook(json!({"turns": [{"content": "late", "delay_ms": 20000}]}));
    let cfg = config(&play, "");
    let err_path = common::unique_path("outcomes", "log");
    let started = Instant::now();
    let mut agentd = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        // Stuck after half a second without events or pongs.
        .env("AGENTD_PROGRESS_TIMEOUT_MS", "500")
        .env("AGENTD_PONG_TIMEOUT_MS", "500")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn");
    let log = || std::fs::read_to_string(&err_path).unwrap_or_default();
    let pid = loop {
        if let Some(pid) = events(&log(), "subagent.spawn")
            .first()
            .and_then(|e| e["pid"].as_i64())
        {
            break pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "no child: {}",
            log()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    unsafe { libc::kill(pid as i32, libc::SIGSTOP) };
    let status = loop {
        if let Some(s) = agentd.try_wait().unwrap() {
            break s;
        }
        if started.elapsed() > Duration::from_secs(25) {
            let _ = agentd.kill();
            let _ = agentd.wait();
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            panic!("the wait was never answered:\n{}", log());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let log = log();
    assert!(!events(&log, "child.unhealthy").is_empty(), "{log}");
    let result = events(&log, "subagent.result");
    assert_eq!(result.len(), 1, "{log}");
    assert_eq!(result[0]["status"], "failed", "{log}");
    assert!(!status.success(), "the run failed with its child: {log}");
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
    let _ = std::fs::remove_file(&err_path);
}

/// A child's deadline bounds its model call in flight: a call that would
/// outlast it is cut off, and the child ends `deadline` when its deadline
/// passes, not a minute later when the supervisor's backstop would.
#[test]
fn a_childs_deadline_cuts_off_its_model_call() {
    let play = playbook(json!({"turns": [{"content": "late", "delay_ms": 20000}]}));
    let cfg = config(&play, ", limits: { deadline: 2s }");
    let started = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr);
    assert!(
        started.elapsed() < Duration::from_secs(12),
        "the call ran out its own length: {:?}\n{log}",
        started.elapsed()
    );
    let result = events(&log, "subagent.result");
    assert_eq!(result.len(), 1, "{log}");
    assert_eq!(result[0]["status"], "deadline", "{log}");
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
}

/// Handles carry on after a restart. The counter began at 0 in every
/// process, so the second life's first child was handed `sub-1` again while
/// the first life's `sub-1` record stood, and replaced it.
#[test]
fn a_restart_does_not_hand_out_a_restored_handle_again() {
    let play = playbook(json!({"turns": [{"content": "done"}]}));
    let store = common::unique_path("outcomes-store", "d");
    let life = |workflow: &str| {
        let path = common::unique_path("outcomes", "yaml");
        std::fs::write(
            &path,
            format!(
                "agent: {{ name: o }}\nstore: {{ kind: file, file: {{ path: {store} }} }}\nobservability: {{ log_level: info }}\n\
                 intelligence: {{ endpoints: \"mock:file:{play}\", model: m }}\n\
                 workflows:\n  - name: {workflow}\n    steps:\n\
                 \x20     s: {{ kind: once }}\n\
                 \x20     sub: {{ kind: subagent, depends_on: [s], durable: true, instruction: \"Say done.\" }}\n\
                 \x20     f: {{ kind: finish, depends_on: [sub], status: completed }}\n"
            ),
        )
        .unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
            .args(["--config", &path])
            .stdin(Stdio::null())
            .output()
            .expect("run");
        let _ = std::fs::remove_file(&path);
        String::from_utf8_lossy(&out.stderr).to_string()
    };
    let handle = |log: &str| {
        events(log, "subagent.spawn")
            .first()
            .and_then(|e| e["handle"].as_str().map(str::to_string))
            .unwrap_or_else(|| panic!("no spawn: {log}"))
    };
    let first = life("one");
    assert_eq!(handle(&first), "sub-1", "{first}");
    let second = life("two");
    assert_eq!(handle(&second), "sub-2", "{second}");
    let _ = std::fs::remove_dir_all(&store);
    let _ = std::fs::remove_file(&play);
}

/// The reason every kill below gives.
const REASON: &str = "operator says stop";

/// A job that spawns one `mode` child, sends it `send` when given, kills it
/// `nap` later, and waits on it from before the kill.
fn kill_config(playbook: &str, mode: &str, send: Option<&str>, nap: &str) -> String {
    let path = common::unique_path("outcomes-kill", "yaml");
    let (say, after) = match send {
        Some(m) => (
            format!(
                "\x20     say: {{ kind: tool, depends_on: [sub], name: subagent.send, args: {{ handle: \"{{{{steps.sub.output.handle}}}}\", message: \"{m}\" }} }}\n"
            ),
            "say",
        ),
        None => (String::new(), "sub"),
    };
    std::fs::write(
        &path,
        format!(
            "agent: {{ name: o }}\nstore: {{ kind: memory }}\nobservability: {{ log_level: info }}\n\
             intelligence: {{ endpoints: \"mock:file:{playbook}\", model: m }}\n\
             workflows:\n  - name: w\n    steps:\n\
             \x20     s: {{ kind: once }}\n\
             \x20     sub: {{ kind: subagent, depends_on: [s], mode: {mode}, instruction: \"Keep going.\" }}\n\
             \x20     w: {{ kind: wait, depends_on: [sub], on: subagent, subagent: \"{{{{steps.sub.output.handle}}}}\", timeout: 60s }}\n\
             {say}\
             \x20     nap: {{ kind: sleep, depends_on: [{after}], duration: {nap} }}\n\
             \x20     kill: {{ kind: tool, depends_on: [nap], name: subagent.kill, args: {{ handle: \"{{{{steps.sub.output.handle}}}}\", reason: \"{REASON}\" }} }}\n\
             \x20     f: {{ kind: finish, depends_on: [w, kill], status: completed }}\n"
        ),
    )
    .unwrap();
    path
}

/// Run `cfg` until it exits: its whole log, and how long after the kill the
/// test saw it exited.
fn run_kill(cfg: &str) -> (String, Duration) {
    let err_path = common::unique_path("outcomes-kill", "log");
    let started = Instant::now();
    let mut agentd = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&err_path).unwrap()))
        .spawn()
        .expect("spawn");
    let log = || std::fs::read_to_string(&err_path).unwrap_or_default();
    let mut killed: Option<Instant> = None;
    loop {
        if killed.is_none() && !events(&log(), "subagent.kill").is_empty() {
            killed = Some(Instant::now());
        }
        if agentd.try_wait().unwrap().is_some() {
            break;
        }
        if started.elapsed() > Duration::from_secs(45) {
            let _ = agentd.kill();
            let _ = agentd.wait();
            panic!("the daemon never exited:\n{}", log());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let log = log();
    let _ = std::fs::remove_file(&err_path);
    let killed = killed.unwrap_or_else(|| panic!("no kill:\n{log}"));
    (log, killed.elapsed())
}

/// The one `subagent.result` line, which must say `cancelled` with the kill's
/// reason, and the one `child.exit` line, whose outcome it returns.
fn killed_outcome(log: &str) -> String {
    let result = events(log, "subagent.result");
    assert_eq!(result.len(), 1, "{log}");
    assert_eq!(result[0]["status"], "cancelled", "{log}");
    assert_eq!(result[0]["err"], REASON, "{log}");
    // The wait on it, asked before the kill, was answered with the kill.
    assert!(
        events(log, "step.done")
            .iter()
            .any(|e| e["step"] == "w" && e["status"] == "failed"),
        "{log}"
    );
    assert!(!log.contains("wait timed out"), "{log}");
    let exits = events(log, "child.exit");
    assert_eq!(exits.len(), 1, "{log}");
    exits[0]["outcome"].as_str().unwrap_or_default().to_string()
}

/// The tokens a killed child left charged: its record's, from its
/// `subagent.result` line, and the daemon's own count of what its children
/// reported, `(in, out)` from its exit line.
fn charged(log: &str) -> (Value, (Value, Value)) {
    let result = events(log, "subagent.result");
    let exit = events(log, "proc.exit");
    assert_eq!(exit.len(), 1, "{log}");
    (
        result[0]["tokens"].clone(),
        (exit[0]["tokens_in"].clone(), exit[0]["tokens_out"].clone()),
    )
}

/// A child inside a model call cannot read the cancel until the call
/// returns, so the kill's ladder ends it: `SIGTERM` to its group at the
/// grace, said once in its trace. The daemon exits seconds after the kill,
/// not when the minute-long call would have returned. The call the child
/// finished before it is charged, and its record says so, though the run it
/// was part of never ended.
#[test]
fn a_kill_ends_a_child_inside_a_model_call_with_its_ladder() {
    let play = playbook(json!({"turns": [
        {"tool_calls": [{"name": "nothing.here", "arguments": {}}],
         "usage": {"prompt_tokens": 1000, "completion_tokens": 200}},
        {"content": "late", "delay_ms": 60000}
    ]}));
    let cfg = kill_config(&play, "async", None, "3s");
    let (log, after_kill) = run_kill(&cfg);
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
    assert!(
        after_kill >= Duration::from_secs(4) && after_kill < Duration::from_secs(20),
        "the child ended {after_kill:?} after the kill, not at its grace:\n{log}"
    );
    assert_eq!(killed_outcome(&log), "Signaled(15)", "{log}");
    let escalated = events(&log, "subagent.kill.escalated");
    assert_eq!(escalated.len(), 1, "{log}");
    assert_eq!(escalated[0]["signal"], "SIGTERM", "{log}");
    assert_eq!(escalated[0]["handle"], "sub-1", "{log}");
    let spawned = events(&log, "subagent.spawn");
    assert_eq!(
        escalated[0]["trace_id"], spawned[0]["trace_id"],
        "in the subagent's trace: {}",
        escalated[0]
    );
    assert!(!events(&log, "lifecycle.idle_exit").is_empty(), "{log}");
    assert_eq!(
        charged(&log),
        (json!(1200), (json!(1000), json!(200))),
        "{log}"
    );
}

/// A warm child killed inside its second turn's model call: the calls its
/// turns finished, the cut-off turn's first included, are charged, and its
/// record carries them, though it never reported that turn's end.
#[test]
fn a_kill_inside_a_warm_turn_charges_the_calls_it_finished() {
    let play = playbook(json!({
        "turns": [{"content": "ready", "usage": {"prompt_tokens": 100, "completion_tokens": 20}}],
        "match": [
            {"when_contains": "\"role\":\"tool\"", "content": "late", "delay_ms": 60000},
            {"when_contains": "the second thing",
             "tool_calls": [{"name": "nothing.here", "arguments": {}}],
             "usage": {"prompt_tokens": 1000, "completion_tokens": 200}}
        ]
    }));
    let cfg = kill_config(&play, "warm", Some("Now the second thing."), "3s");
    let (log, _) = run_kill(&cfg);
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
    assert_eq!(events(&log, "subagent.turn").len(), 1, "{log}");
    assert_eq!(killed_outcome(&log), "Signaled(15)", "{log}");
    assert_eq!(
        charged(&log),
        (json!(1320), (json!(1100), json!(220))),
        "{log}"
    );
}

/// A warm child waiting for its next message is between steps: it reads the
/// cancel and exits inside the grace, and is never signalled.
#[test]
fn a_kill_stops_a_child_between_steps_on_its_cancel() {
    let play = playbook(json!({"turns": [{"content": "ready"}]}));
    let cfg = kill_config(&play, "warm", None, "2s");
    let (log, after_kill) = run_kill(&cfg);
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
    assert_eq!(
        events(&log, "subagent.turn").len(),
        1,
        "the child finished its first turn before the kill:\n{log}"
    );
    assert!(
        after_kill < Duration::from_secs(4),
        "the child ran {after_kill:?} past the kill:\n{log}"
    );
    assert!(killed_outcome(&log).starts_with("Exited("), "{log}");
    assert!(events(&log, "subagent.kill.escalated").is_empty(), "{log}");
}

/// A child whose model call returns inside the grace reports a completed
/// result; the kill came first, so the record stays `cancelled`, and the
/// child, gone before the grace ran out, is never signalled.
#[test]
fn a_result_a_killed_child_reports_in_its_grace_leaves_it_cancelled() {
    let play = playbook(json!({"turns": [{"content": "late", "delay_ms": 4000}]}));
    let cfg = kill_config(&play, "async", None, "2s");
    let (log, _) = run_kill(&cfg);
    let _ = std::fs::remove_file(&cfg);
    let _ = std::fs::remove_file(&play);
    assert!(killed_outcome(&log).starts_with("Exited("), "{log}");
    assert!(events(&log, "subagent.kill.escalated").is_empty(), "{log}");
}

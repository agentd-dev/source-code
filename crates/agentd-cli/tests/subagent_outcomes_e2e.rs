// SPDX-License-Identifier: AGPL-3.0-only
//! A subagent's record ends with the child, however the child ends: on one
//! of its budgets, or killed without a word. A record left looking live held
//! a `limits.subagents.breadth` slot for ever, never answered a wait on it,
//! and was spawned again at the next start.
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

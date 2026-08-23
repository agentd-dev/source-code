// SPDX-License-Identifier: AGPL-3.0-only
//! agentd **goal watchdog** (RFC 0026) end to end: a periodic supervisor-level
//! check with a CEL condition. When the goal is achieved the daemon self-finishes
//! (drains, exits 0); when no progress is made for `stuck_after` checks it
//! self-corrects by firing the configured recovery workflow.
//!
//! The CEL condition path needs `--features cel`.
#![cfg(all(unix, feature = "cel"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
    let pb = common::unique_path("goal-playbook", "json");
    std::fs::write(&pb, playbook.to_string()).unwrap();
    let addr_file = common::unique_path("goal-mock-llm", "addr");
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

struct Daemon {
    child: Option<Child>,
    stderr_path: String,
}
impl Daemon {
    fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }
    fn events(&self, name: &str) -> Vec<Value> {
        self.stderr()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == name)
            .collect()
    }
    /// Poll until the daemon exits on its own; returns its exit code.
    fn wait_self_exit(&mut self, secs: u64) -> Option<i32> {
        let child = self.child.as_mut()?;
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if let Ok(Some(s)) = child.try_wait() {
                return s.code();
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }
    fn wait_for<F: Fn(&Daemon) -> bool>(&self, f: F, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if f(self) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.stderr_path);
    }
}
fn spawn_daemon(config: &str) -> Daemon {
    let stderr_path = common::unique_path("goal-daemon", "log");
    let errf = std::fs::File::create(&stderr_path).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn goal daemon");
    Daemon {
        child: Some(child),
        stderr_path,
    }
}

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("agentd-goal", "yaml");
    std::fs::write(&path, yaml).unwrap();
    path
}

#[test]
fn a_met_goal_condition_self_finishes_the_daemon() {
    let llm = spawn_mock_llm(
        &json!({"turns": [{"content": "did the work"}, {"content": "did the work"}]}),
    );
    // A daemon whose `once` workflow finishes one run; the goal watchdog then sees
    // `runs_finished >= 1` and finishes (drains → exit 0) with no SIGTERM.
    let cfg = write_config(&format!(
        "config_version: \"1\"\n\
         agent:\n  name: g1\n  instruction: You do the work.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         goal:\n  statement: at least one run has finished\n  check: {{every: 1s, condition: \"state.counters.runs_finished >= 1\"}}\n  on_achieved: finish\n\
         workflows:\n  - name: work\n    steps:\n      s: {{kind: once}}\n      a: {{kind: agent, depends_on: [s], instruction: \"do it\"}}\n      f: {{kind: finish, depends_on: [a]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n",
        llm.uri
    ));
    let mut daemon = spawn_daemon(&cfg);
    let code = daemon.wait_self_exit(15);
    assert_eq!(
        code,
        Some(0),
        "the goal watchdog should finish the daemon with exit 0; stderr:\n{}",
        daemon.stderr()
    );
    assert!(
        !daemon.events("goal.achieved").is_empty(),
        "the goal was judged achieved:\n{}",
        daemon.stderr()
    );
    assert!(
        daemon
            .events("drain.start")
            .iter()
            .any(|e| e["reason"].as_str().is_some_and(|r| r.contains("goal"))),
        "the drain was triggered by the goal watchdog:\n{}",
        daemon.stderr()
    );
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn a_stuck_goal_self_corrects_by_firing_the_recovery_workflow() {
    let llm =
        spawn_mock_llm(&json!({"turns": [{"content": "recovered"}, {"content": "recovered"}]}));
    // The goal is never achievable and nothing makes progress, so after
    // `stuck_after` checks the watchdog fires the `recover` workflow.
    let cfg = write_config(&format!(
        "config_version: \"1\"\n\
         agent:\n  name: g2\n  instruction: You recover.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         goal:\n  statement: unreachable\n  check: {{every: 400ms, condition: \"false\"}}\n  stuck_after: 2\n  on_stuck: {{workflow: recover}}\n\
         workflows:\n  - name: recover\n    steps:\n      m: {{kind: manual}}\n      a: {{kind: agent, depends_on: [m], instruction: \"recover\"}}\n      f: {{kind: finish, depends_on: [a]}}\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n",
        llm.uri
    ));
    let daemon = spawn_daemon(&cfg);
    assert!(
        daemon.wait_for(|d| !d.events("goal.stuck").is_empty(), 10),
        "the watchdog detected the stuck goal:\n{}",
        daemon.stderr()
    );
    assert!(
        daemon.wait_for(
            |d| d
                .events("start.fired")
                .iter()
                .any(|e| e["kind"] == "goal" && e["workflow"] == "recover"),
            10
        ),
        "the stuck goal fired the recovery workflow:\n{}",
        daemon.stderr()
    );
    assert!(
        daemon.wait_for(
            |d| d
                .events("run.done")
                .iter()
                .any(|e| e["status"] == "completed"),
            10
        ),
        "the recovery workflow ran to completion:\n{}",
        daemon.stderr()
    );
    std::fs::remove_file(&cfg).ok();
}

#[test]
fn an_llm_judge_decides_the_goal_is_achieved_and_finishes() {
    // No CEL condition ⇒ `check.via: agent` forces the LLM judge. The mock returns
    // an "achieved" verdict when it sees the judge prompt (which contains "GOAL:"),
    // so the watchdog finishes the daemon (drains → exit 0).
    let llm = spawn_mock_llm(&json!({
        "turns": [{"content": "unused"}],
        "match": [{"when_contains": "GOAL:", "content": "{\"achieved\": true, \"stuck\": false, \"reason\": \"the task is complete\"}"}]
    }));
    let cfg = write_config(&format!(
        "config_version: \"1\"\n\
         agent:\n  name: gj\n  instruction: You do the work.\n  preflight: never\n\
         intelligence:\n  endpoints: {}\n  model: mock\n\
         store:\n  kind: memory\n\
         goal:\n  statement: the task is complete\n  check: {{every: 1s, via: agent}}\n  on_achieved: finish\n\
         lifecycle:\n  run_until: drained\n\
         observability:\n  log_level: info\n",
        llm.uri
    ));
    let mut daemon = spawn_daemon(&cfg);
    let code = daemon.wait_self_exit(15);
    assert_eq!(
        code,
        Some(0),
        "the LLM judge should finish the daemon with exit 0; stderr:\n{}",
        daemon.stderr()
    );
    assert!(
        !daemon.events("goal.judge.start").is_empty(),
        "the LLM judge ran:\n{}",
        daemon.stderr()
    );
    assert!(
        daemon
            .events("goal.achieved")
            .iter()
            .any(|e| e["via"] == "judge"),
        "the goal was judged achieved by the LLM:\n{}",
        daemon.stderr()
    );
    std::fs::remove_file(&cfg).ok();
}

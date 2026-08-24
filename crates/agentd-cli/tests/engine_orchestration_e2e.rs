// SPDX-License-Identifier: AGPL-3.0-only
//! Workflow engine v3 orchestration: start-node triggers (`loop`, `schedule`,
//! `subscribe`, `signal`, `event`), the `workflow` child-run node with
//! `sync`/`cascade`, `wait`/`join` on runs, `workflow.signal`/`wait`, step
//! `cache`, and the `think` presets — all driven through the real binary. CEL
//! is used throughout.
#![cfg(all(feature = "cel", feature = "cron"))]

mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("engine-orch", "yaml");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    path
}

fn run_agentd(config: &str, extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentd"));
    cmd.args(["--config", config]);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null()).output().expect("run agentd")
}

fn events(stderr: &str, name: &str) -> Vec<serde_json::Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

#[test]
fn a_loop_start_reruns_until_a_condition_and_an_event_start_reacts_to_completion() {
    // A `loop` workflow increments a counter each run and stops (`until`) at 3;
    // an `event` workflow fires on every workflow.finished and appends to memory.
    let steps_loop = r#"{
        "tick": {"kind": "loop", "until": "CEL: last.count >= 3", "interval": "10ms", "max_iterations": 20},
        "count": {"kind": "assign", "depends_on": ["tick"], "value": "CEL: has(memory.n) ? memory.n + 1 : 1", "writes": "c"},
        "save": {"kind": "memory.set", "depends_on": ["count"], "key": "n", "value": "{{vars.c}}"},
        "done": {"kind": "finish", "depends_on": ["save"], "status": "completed", "output": {"count": "{{vars.c}}"}}
    }"#;
    let steps_event = r#"{
        "on_finish": {"kind": "event", "on": "workflow.finished", "filter": "CEL: payload.workflow == 'looper'"},
        "bump": {"kind": "memory.set", "depends_on": ["on_finish"], "key": "finishes", "value": "CEL: has(memory.finishes) ? memory.finishes + 1 : 1"},
        "done": {"kind": "finish", "depends_on": ["bump"], "status": "completed"}
    }"#;
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: loops\nstore:\n  kind: memory\nworkflows:\n  - name: looper\n    steps: {steps_loop}\n  - name: reactor\n    steps: {steps_event}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n"
    ));
    let out = run_agentd(&cfg, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    // The loop fired exactly three iterations then stopped on `until`.
    let looper_done: Vec<serde_json::Value> = events(&stderr, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "looper")
        .collect();
    assert_eq!(looper_done.len(), 3, "three loop iterations: {stderr}");
    assert_eq!(looper_done[2]["output"]["count"], 3);
    assert!(
        events(&stderr, "start.loop.stopped")
            .iter()
            .any(|e| e["reason"] == "until"),
        "{stderr}"
    );
    // The event workflow reacted to each of the three finishes.
    let reactor_done: Vec<serde_json::Value> = events(&stderr, "run.done")
        .into_iter()
        .filter(|e| e["workflow"] == "reactor")
        .collect();
    assert_eq!(
        reactor_done.len(),
        3,
        "the event start fired thrice: {stderr}"
    );
}

#[test]
fn a_schedule_start_fires_on_an_interval() {
    let steps = r#"{
        "every": {"kind": "schedule", "every": "60ms"},
        "note": {"kind": "assign", "depends_on": ["every"], "value": "CEL: run.start.node", "writes": "n"},
        "done": {"kind": "finish", "depends_on": ["note"], "status": "completed"}
    }"#;
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: sched\nstore:\n  kind: memory\nworkflows:\n  - name: cron\n    steps: {steps}\nlifecycle:\n  run_until: drained\n  drain_timeout: 3s\nobservability:\n  log_level: info\n"
    ));
    // The daemon runs; kill it after ~400ms and count the firings.
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    std::thread::sleep(Duration::from_millis(400));
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "clean drain; stderr:\n{stderr}");
    let fires = events(&stderr, "start.fired")
        .into_iter()
        .filter(|e| e["kind"] == "schedule")
        .count();
    assert!(
        (3..=12).contains(&fires),
        "≈ every 60ms over ~400ms, got {fires}: {stderr}"
    );
}

#[test]
fn a_parent_workflow_runs_a_child_synchronously_and_a_signal_coordinates_two_runs() {
    let child = r#"{
        "s": {"kind": "manual"},
        "work": {"kind": "assign", "depends_on": ["s"], "value": "CEL: inputs.x * 10", "writes": "y"},
        "signal": {"kind": "workflow.signal", "depends_on": ["work"], "name": "child-done", "payload": {"y": "{{vars.y}}"}},
        "done": {"kind": "finish", "depends_on": ["signal"], "status": "completed", "output": {"y": "{{vars.y}}"}}
    }"#;
    let parent = r#"{
        "start": {"kind": "once"},
        "spawn": {"kind": "workflow", "depends_on": ["start"], "name": "child", "mode": "sync", "inputs": {"x": 5}},
        "waited": {"kind": "assign", "depends_on": ["spawn"], "value": "{{steps.spawn.output.output.y}}", "writes": "got"},
        "await_signal": {"kind": "wait", "depends_on": ["start"], "on": "signal", "signal": "child-done", "timeout": "5s"},
        "done": {"kind": "finish", "depends_on": ["waited", "await_signal"], "status": "completed",
                 "output": {"child_y": "{{vars.got}}", "signal_y": "{{steps.await_signal.output.payload.y}}"}}
    }"#;
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: parent\nworkflows:\n  - name: child\n    steps: {child}\n  - name: parent\n    steps: {parent}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n"
    ));
    let out = run_agentd(&cfg, &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr:\n{stderr}");
    // The child ran (a run started by the workflow node) and the parent read its output.
    let parent_done = events(&stderr, "run.done")
        .into_iter()
        .find(|e| e["workflow"] == "parent")
        .expect("parent finished");
    assert_eq!(parent_done["status"], "completed", "{stderr}");
    assert_eq!(parent_done["output"]["child_y"], 50, "{parent_done}");
    assert_eq!(
        parent_done["output"]["signal_y"], 50,
        "the wait resolved on the child's signal: {parent_done}"
    );
    assert!(
        events(&stderr, "run.start")
            .iter()
            .any(|e| e["workflow"] == "child"),
        "{stderr}"
    );
}

#[test]
fn step_cache_memoizes_and_think_presets_classify_via_the_mock_llm() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    // A mock LLM playbook: the classify preset gets a JSON verdict.
    let pb = common::unique_path("pb", "json");
    std::fs::write(&pb, serde_json::json!({
        "match": [{"when_contains": "Classify the input", "content": {"class": "bug", "confidence": 0.9, "reason": "stack trace"}}],
        "turns": [{"content": "{}"}]
    }).to_string()).unwrap();
    let llm_addr = common::unique_path("mock-llm", "addr");
    let _ = std::fs::remove_file(&llm_addr);
    let mut llm = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--internal-mock-llm", &llm_addr, &format!("file:{pb}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let llm_uri = format!("http://{}", common::read_addr_file(&llm_addr));
    // `cache` on the classify step: life 1 calls the model, life 2 is a cache
    // hit (no model call). A memory.set records the run count.
    let steps = r#"{
        "start": {"kind": "once", "policy": "always"},
        "mark": {"kind": "memory.set", "depends_on": ["start"], "key": "calls", "value": "CEL: has(memory.calls) ? memory.calls + 1 : 1"},
        "class": {"kind": "classify", "depends_on": ["mark"], "input": "the app crashed with a stack trace", "classes": ["bug", "question", "chore"], "cache": {"key": "constant"}},
        "done": {"kind": "finish", "depends_on": ["class"], "status": "completed", "output": {"calls": "{{memory.calls}}", "class": "{{steps.class.output.class}}"}}
    }"#;
    let cfg = write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: cacher\nintelligence:\n  endpoints: {llm_uri}\n  model: mock\nmcp:\n  servers:\n    - name: mock\n      endpoint: {}\nstore:\n  kind: mcp\n  mcp:\n    server: mock\nworkflows:\n  - name: pipe\n    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: info\n  log_content: true\n",
        mock.uri()
    ));
    // Life 1: the classify step calls the model and is memoized.
    let out1 = run_agentd(&cfg, &[]);
    let stderr1 = String::from_utf8_lossy(&out1.stderr);
    assert_eq!(out1.status.code(), Some(0), "stderr:\n{stderr1}");
    let d1 = events(&stderr1, "run.done");
    assert_eq!(d1[0]["output"]["calls"], 1);
    assert_eq!(
        d1[0]["output"]["class"], "bug",
        "the classify preset returned the mock verdict"
    );
    assert!(
        events(&stderr1, "step.cache_hit").is_empty(),
        "first run computes"
    );
    assert_eq!(
        events(&stderr1, "step.turn.spawn")
            .iter()
            .filter(|e| e["step"] == "class")
            .count(),
        1,
        "life 1 calls the model"
    );
    // Life 2 (same store): the classify step is a cache hit — no model call —
    // and memory.calls advanced to 2.
    let out2 = run_agentd(&cfg, &[]);
    let stderr2 = String::from_utf8_lossy(&out2.stderr);
    assert_eq!(out2.status.code(), Some(0), "stderr:\n{stderr2}");
    assert!(
        events(&stderr2, "step.cache_hit")
            .iter()
            .any(|e| e["step"] == "class"),
        "second run hits the cache: {stderr2}"
    );
    assert!(
        events(&stderr2, "step.turn.spawn")
            .iter()
            .all(|e| e["step"] != "class"),
        "life 2 does not call the model: {stderr2}"
    );
    let d2 = events(&stderr2, "run.done");
    assert_eq!(d2[0]["output"]["calls"], 2, "memory advanced: {d2:?}");
    assert_eq!(
        d2[0]["output"]["class"], "bug",
        "the memoized verdict: {d2:?}"
    );
    let _ = llm.kill();
    let _ = llm.wait();
}

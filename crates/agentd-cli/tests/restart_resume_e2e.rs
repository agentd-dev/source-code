// SPDX-License-Identifier: AGPL-3.0-only
//! The restart contract, at its harshest: SIGKILL a daemon mid-run — no drain,
//! no goodbye checkpoint beyond the ones the engine already wrote — and the
//! next life picks the run up and finishes it. Three lives of increasing
//! hostility:
//!
//! 1. same config: resume — the baseline the store promises.
//! 2. definition CHANGED: the run finishes under the definition it STARTED
//!    with (the durable pin), while a fresh run uses the new one.
//! 3. definition REMOVED: the run still finishes (pin again), then the pin
//!    is garbage-collected.
//!
//! Before durable pins, 2 and 3 ended in `run.refused` — a restart plus a
//! config edit could strand work the daemon had already promised to do.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

fn config(dir: &str, marker: &str, with_wf: bool) -> String {
    let wf = if with_wf {
        format!(
            "  - name: slow\n    steps:\n\
             \x20     s:   {{kind: once, policy: always}}\n\
             \x20     nap: {{kind: sleep, depends_on: [s], duration: 1200ms}}\n\
             \x20     out: {{kind: assign, depends_on: [nap], value: \"{marker}\"}}\n\
             \x20     f:   {{kind: finish, depends_on: [out], status: completed, output: \"{{{{steps.out.output}}}}\"}}\n"
        )
    } else {
        String::new()
    };
    format!(
        "config_version: \"2\"\nagent:\n  name: phoenix\n\
         store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
         workflows:\n  - name: idle\n    steps:\n\
         \x20     s: {{kind: manual}}\n\
         \x20     f: {{kind: finish, depends_on: [s]}}\n{wf}\
         lifecycle:\n  run_until: idle\n  idle_grace: 700ms\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

/// Spawn; wait until the run is mid-sleep; SIGKILL. Returns this life's log.
fn kill_mid_run(cfg: &str) -> String {
    let err_path = common::unique_path("phoenix", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let mut child: Child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .spawn()
        .expect("spawn");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let log = std::fs::read_to_string(&err_path).unwrap_or_default();
        if events(&log, "step.start")
            .iter()
            .any(|e| e["step"] == "nap")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "run never reached the sleep:\n{log}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    unsafe { libc::kill(child.id() as i32, libc::SIGKILL) };
    let _ = child.wait();
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    log
}

/// Run a full life to drained-exit; return (exit, log).
fn life(cfg: &str) -> (Option<i32>, String) {
    let err_path = common::unique_path("phoenix", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .status()
        .expect("run");
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    (out.code(), log)
}

#[test]
fn sigkill_then_same_config_resumes_and_completes() {
    let dir = common::unique_path("phx-same", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, config(&dir, "v1", true)).unwrap();
    let l1 = kill_mid_run(&cfg);
    assert!(events(&l1, "run.done").is_empty(), "killed mid-run:\n{l1}");
    let (code, l2) = life(&cfg);
    assert_eq!(code, Some(0), "{l2}");
    assert!(
        events(&l2, "run.done")
            .iter()
            .any(|e| e["workflow"] == "slow" && e["status"] == "completed" && e["output"] == "v1"),
        "the killed run resumed and completed:\n{l2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sigkill_then_changed_definition_finishes_old_run_under_its_pin() {
    let dir = common::unique_path("phx-chg", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, config(&dir, "old-def", true)).unwrap();
    let l1 = kill_mid_run(&cfg);
    drop(l1);
    // The definition changes while the daemon is dead.
    std::fs::write(&cfg, config(&dir, "new-def", true)).unwrap();
    let (code, l2) = life(&cfg);
    assert_eq!(code, Some(0), "{l2}");
    let done = events(&l2, "run.done");
    // The restored run finished under the definition it STARTED with…
    assert!(
        done.iter()
            .any(|e| e["workflow"] == "slow" && e["output"] == "old-def"),
        "pin restored, old run completes as authored:\n{l2}"
    );
    assert!(
        !events(&l2, "run.refused")
            .iter()
            .any(|e| e["run"].as_str().is_some_and(|r| r.starts_with("slow-"))),
        "no refusal:\n{l2}"
    );
    assert!(
        !events(&l2, "workflow.pin_restored").is_empty(),
        "the durable pin was read back:\n{l2}"
    );
    // …and the NEW definition serves the new `once` run of this life.
    assert!(
        done.iter()
            .any(|e| e["workflow"] == "slow" && e["output"] == "new-def"),
        "the new definition runs too:\n{l2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sigkill_then_removed_definition_still_finishes_the_run() {
    let dir = common::unique_path("phx-rm", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, config(&dir, "orphan", true)).unwrap();
    let l1 = kill_mid_run(&cfg);
    drop(l1);
    // The workflow is gone from the config entirely.
    std::fs::write(&cfg, config(&dir, "orphan", false)).unwrap();
    let (code, l2) = life(&cfg);
    assert_eq!(code, Some(0), "{l2}");
    assert!(
        events(&l2, "run.done")
            .iter()
            .any(|e| e["workflow"] == "slow"
                && e["status"] == "completed"
                && e["output"] == "orphan"),
        "the orphaned run still completed under its durable pin:\n{l2}"
    );
    assert!(!events(&l2, "workflow.pin_restored").is_empty(), "{l2}");
    // Once it landed, the pin was released.
    assert!(
        events(&l2, "workflow.unloaded")
            .iter()
            .any(|e| e["workflow"] == "slow"),
        "pin GC after the last run:\n{l2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

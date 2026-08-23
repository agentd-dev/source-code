// SPDX-License-Identifier: AGPL-3.0-only
//! Durability classes end to end: `durable: false` on a workflow keeps its
//! runs out of the store entirely (the fast path), `store.durability.work:
//! ephemeral` flips the deployment default with `durable: true` opting back
//! in, and a `durable: false` subagent spawn leaves no record.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

/// Run a config against a file store rooted in `dir`/state and return
/// (exit, stderr log, every state-file path+content concatenated).
fn run_and_dump_state(cfg_text: &str) -> (Option<i32>, String, String) {
    let dir = common::unique_path("durab", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let mut state = String::new();
    let mut stack = vec![std::path::PathBuf::from(format!("{dir}/state"))];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                stack.push(e.path());
            }
        } else if p.is_file() {
            state.push_str(&p.display().to_string());
            state.push('\n');
            state.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
            state.push('\n');
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log, state)
}

#[test]
fn a_non_durable_workflow_leaves_no_run_record() {
    let (code, log, state) = run_and_dump_state(
        "config_version: \"1\"\nagent: { name: d }\nstore: { kind: file, file: { path: __STATE__ } }\n\
         lifecycle: { run_until: idle, idle_grace: 500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n\
        \x20 - name: w_fast\n    durable: false\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     f: { kind: finish, depends_on: [s], status: completed, output: fast }\n\
        \x20 - name: w_keep\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     f: { kind: finish, depends_on: [s], status: completed, output: keep }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.matches("\"event\":\"run.done\"").count() >= 2,
        "both workflows ran:\n{log}"
    );
    assert!(
        state.contains("w_keep"),
        "the durable run IS in the store:\n{state}"
    );
    assert!(
        !state.contains("w_fast"),
        "the non-durable run left NOTHING in the store:\n{state}"
    );
}

#[test]
fn ephemeral_work_default_flips_and_explicit_durable_opts_back_in() {
    let (code, log, state) = run_and_dump_state(
        "config_version: \"1\"\nagent: { name: d }\nstore: { kind: file, file: { path: __STATE__ }, durability: { work: ephemeral } }\n\
         lifecycle: { run_until: idle, idle_grace: 500ms }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n\
        \x20 - name: w_default\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     f: { kind: finish, depends_on: [s], status: completed }\n\
        \x20 - name: w_pinned\n    durable: true\n    steps:\n\
        \x20     s: { kind: once }\n\
        \x20     f: { kind: finish, depends_on: [s], status: completed }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !state.contains("w_default"),
        "under work: ephemeral, the default is memory-only:\n{state}"
    );
    assert!(
        state.contains("w_pinned"),
        "`durable: true` opts a workflow back into persistence:\n{state}"
    );
}

#[test]
fn a_non_durable_subagent_leaves_no_record() {
    let (code, log, state) = run_and_dump_state(
        "config_version: \"1\"\nagent: { name: d }\nstore: { kind: file, file: { path: __STATE__ } }\n\
         intelligence: { endpoints: \"mock:final\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 900ms }\n\
         observability: { log_level: info, log_content: true }\n\
         workflows:\n  - name: w\n    steps:\n\
        \x20     s:  { kind: once }\n\
        \x20     throwaway: { kind: subagent, depends_on: [s], instruction: \"say ok\", durable: false }\n\
        \x20     kept:      { kind: subagent, depends_on: [throwaway], instruction: \"say ok\" }\n\
        \x20     f:  { kind: finish, depends_on: [kept], status: completed }\n",
    );
    assert_eq!(code, Some(0), "{log}");
    let records: Vec<&str> = state
        .lines()
        .filter(|l| l.contains("/subagent/") && l.ends_with(".json"))
        .collect();
    assert_eq!(
        records.len(),
        1,
        "exactly the durable spawn left a record: {records:?}\n{state}"
    );
}

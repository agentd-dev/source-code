// SPDX-License-Identifier: Apache-2.0
//! The file store as the DEFAULT for a long-lived instance (RFC 0033 §5), end
//! to end through the real binary.
//!
//! Four properties, in the order they matter:
//!   1. a long-lived config with no `store` block STARTS (it used to exit 2) and
//!      says on one line what its durability actually is (`store.file`);
//!   2. an EXPLICIT `store.kind: none` on the same config still exits 2 — the
//!      default must never override a stated choice;
//!   3. a ONE-SHOT config with no `store` block writes NOTHING to disk — the
//!      regression that would surprise every existing user of `agentd --instruction`;
//!   4. state survives a kill: the second life resumes rather than starting
//!      fresh, which is the property the whole RFC exists for.
#![cfg(feature = "workflow")]

mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

/// A long-lived instance: a `schedule` start node fires a trivial durable run.
/// No `agent` step, so no LLM is needed to exercise the store path.
const SCHEDULE_STEPS: &str = r#"{
    "every": {"kind": "schedule", "every": "50ms"},
    "note": {"kind": "memory.set", "depends_on": ["every"], "key": "ticks", "value": "1"},
    "done": {"kind": "finish", "depends_on": ["note"], "status": "completed"}
}"#;

/// A job: one `once` start, no listener, no goal ⇒ the store default must not
/// move.
const ONCE_STEPS: &str = r#"{
    "start": {"kind": "once"},
    "note": {"kind": "memory.set", "depends_on": ["start"], "key": "ticks", "value": "1"},
    "done": {"kind": "finish", "depends_on": ["note"], "status": "completed"}
}"#;

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("store-file-default", "yaml");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    path
}

/// A per-test state root that does not exist yet — the tests assert on whether
/// agentd creates it.
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

/// Run a long-lived daemon for `ms`, then SIGTERM it and collect its stderr.
/// `AGENTD_STATE_DIR` points the file store at a test-owned directory (the
/// default chain would otherwise land in the user's XDG state dir).
fn run_daemon(config: &str, state_dir: &str, ms: u64) -> (Option<i32>, String) {
    let child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", config])
        .env("AGENTD_STATE_DIR", state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd");
    let pid = child.id() as i32;
    std::thread::sleep(Duration::from_millis(ms));
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let out = child.wait_with_output().expect("wait agentd");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_long_lived_instance_with_no_store_block_starts_on_the_file_store() {
    let dir = state_root("ll-default");
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: sched-default\nworkflows:\n  - name: cron\n    steps: {SCHEDULE_STEPS}\nlifecycle:\n  run_until: drained\n  drain_timeout: 3s\nobservability:\n  log_level: info\n"
    ));
    let (code, stderr) = run_daemon(&cfg, &dir, 300);
    // It used to exit 2 here with "store.kind is none but the instance is
    // long-lived"; a clean drain is the whole change.
    assert_eq!(
        code,
        Some(0),
        "starts and drains cleanly; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("store.kind is none"),
        "no refusal: {stderr}"
    );
    // …and it is honest about what that durability is worth (RFC 0033 §5.1).
    let told = events(&stderr, "store.file");
    assert_eq!(told.len(), 1, "logged once at startup: {stderr}");
    assert_eq!(told[0]["defaulted"], true, "nobody wrote store.kind");
    assert_eq!(
        told[0]["path"].as_str(),
        Some(dir.as_str()),
        "the AGENTD_STATE_DIR link of the chain"
    );
    assert_eq!(told[0]["generation"], 1, "the first life");
    assert!(
        told[0]["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("not a move to another host"),
        "the caveat is on the line, not in the docs only: {stderr}"
    );
    // The store is real: the state directory exists and holds the manifest.
    assert!(
        std::path::Path::new(&dir).exists(),
        "the state root was created"
    );
    // The instance keys its state by `agent.name` (RFC 0033 §3), not by a hash
    // of the config: `<root>/<prefix>/<instance>/…`, a path a human can read.
    let inst = std::path::Path::new(&dir)
        .join("agentd")
        .join("sched-default");
    assert!(
        inst.join("manifest").join("agent.json").is_file(),
        "the manifest is on disk under the agent name: {}",
        inst.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_explicit_none_on_a_long_lived_instance_is_still_refused() {
    let dir = state_root("ll-explicit-none");
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: sched-explicit\nstore:\n  kind: none\nworkflows:\n  - name: cron\n    steps: {SCHEDULE_STEPS}\nlifecycle:\n  run_until: drained\n  drain_timeout: 3s\nobservability:\n  log_level: info\n"
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_STATE_DIR", &dir)
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "still exit 2:\n{stderr}");
    assert!(
        stderr.contains("store.kind is none but the instance is long-lived"),
        "the operator's stated choice is refused, not overridden: {stderr}"
    );
    assert!(
        !std::path::Path::new(&dir).exists(),
        "a refusal writes nothing"
    );
}

#[test]
fn a_one_shot_instance_with_no_store_block_still_writes_nothing() {
    let dir = state_root("one-shot");
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: jobby\nworkflows:\n  - name: job\n    steps: {ONCE_STEPS}\nobservability:\n  log_level: info\n"
    ));
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .env("AGENTD_STATE_DIR", &dir)
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "the job runs:\n{stderr}");
    // The one-shot default is untouched: `none`, an in-process store, no disk.
    assert!(
        events(&stderr, "store.file").is_empty(),
        "no file store for a job: {stderr}"
    );
    assert_eq!(
        events(&stderr, "store.none").len(),
        1,
        "still the in-process store: {stderr}"
    );
    assert!(
        !std::path::Path::new(&dir).exists(),
        "a one-shot run creates no state directory (it would surprise every existing user)"
    );
}

#[test]
fn state_survives_a_kill_and_the_second_life_resumes_instead_of_starting_fresh() {
    let dir = state_root("resume");
    // `store.file.path` is the first link of the root chain, so this run pins
    // its own directory regardless of the environment.
    let cfg = write_config(&format!(
        "config_version: \"2\"\nagent:\n  name: resumer\nstore:\n  kind: file\n  file:\n    path: {dir}\nworkflows:\n  - name: cron\n    steps: {SCHEDULE_STEPS}\nlifecycle:\n  run_until: drained\n  drain_timeout: 3s\nobservability:\n  log_level: info\n"
    ));
    // Life 1: nothing on disk ⇒ a fresh restore, then the schedule writes.
    let (code, first) = run_daemon(&cfg, &dir, 300);
    assert_eq!(code, Some(0), "life 1 drains; stderr:\n{first}");
    assert_eq!(
        events(&first, "restore.fresh").len(),
        1,
        "life 1 has nothing to restore: {first}"
    );
    let told = events(&first, "store.file");
    assert_eq!(told.len(), 1, "{first}");
    assert_eq!(
        told[0]["defaulted"], false,
        "this config NAMED the file store"
    );
    assert!(
        events(&first, "start.fired")
            .iter()
            .any(|e| e["kind"] == "schedule"),
        "the schedule actually ran, so there is state to keep: {first}"
    );

    // Life 2: the same directory, a new process (the first one is fully gone, so
    // its exclusive lock is released — RFC 0033 §4.1).
    let (code, second) = run_daemon(&cfg, &dir, 300);
    assert_eq!(code, Some(0), "life 2 drains; stderr:\n{second}");
    assert!(
        events(&second, "restore.fresh").is_empty(),
        "life 2 must NOT start fresh — this is the property RFC 0033 exists for:\n{second}"
    );
    let done = events(&second, "restore.done");
    assert_eq!(done.len(), 1, "life 2 restored a manifest: {second}");
    assert_eq!(
        done[0]["generation"], 2,
        "the generation counts the lives (RFC 0025 §6): {second}"
    );
    assert_eq!(
        events(&second, "store.file")[0]["generation"],
        2,
        "and the startup line reports the life the operator is in"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

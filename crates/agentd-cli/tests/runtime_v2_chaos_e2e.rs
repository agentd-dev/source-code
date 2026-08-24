// SPDX-License-Identifier: AGPL-3.0-only
//! agentd **chaos matrix** (the durability contract): a workflow run is
//! SIGKILLed at each of the runtime's durable-write kill points
//! (`AGENTD_TEST_KILL_AT`), and the next life must **restore and complete it
//! exactly once** — never a lost run, never a double-executed effect. The store
//! is a mock MCP server that outlives both agentd lives (the durable backing).
#![cfg(all(unix, any(feature = "internal-mocks", debug_assertions)))]

mod common;

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};

fn write_config(yaml: &str) -> String {
    let path = common::unique_path("chaos-v2", "yaml");
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

/// A store-backed job whose workflow is a plain `once → noop → noop → noop →
/// finish` chain — deterministic, no model, so the only variable is *when we
/// die*. Every step transition and durable write is exercised.
fn chaos_config(mock_uri: &str) -> String {
    let steps = r#"{
        "s": {"kind": "once"},
        "a": {"kind": "noop", "depends_on": ["s"]},
        "b": {"kind": "noop", "depends_on": ["a"]},
        "c": {"kind": "noop", "depends_on": ["b"]},
        "f": {"kind": "finish", "depends_on": ["c"], "status": "completed", "output": "done"}
    }"#;
    write_config(&format!(
        "config_version: \"1\"\nagent:\n  name: chaos\nmcp:\n  servers:\n    - name: mock\n      endpoint: {mock_uri}\nstore:\n  kind: mcp\n  mcp:\n    server: mock\nworkflows:\n  - name: chain\n    steps: {steps}\nlifecycle:\n  run_until: idle\n  idle_grace: 1s\nobservability:\n  log_level: warn\n"
    ))
}

#[test]
fn a_workflow_run_survives_a_sigkill_at_every_durable_write_point() {
    // The durable-write kill points a workflow run passes through. Killing at each
    // (in a fresh store) and restarting must yield exactly one completed run.
    const KILL_POINTS: &[&str] = &[
        "state.before_put", // dying before a store write commits
        "state.after_put",  // dying after it commits, before the in-memory seq updates
        "step.running",     // dying with a step marked running (replayed on restore)
        "step.before_done", // dying just before a step is committed done
    ];

    for point in KILL_POINTS {
        // A FRESH store per kill point (the mock outlives both lives below).
        let mock = common::spawn_mock_mcp("mock://noop", false);
        let cfg = chaos_config(&mock.uri());

        // Life 1: die at the kill point (SIGKILL from inside the process).
        let out1 = run_agentd(&cfg, &[("AGENTD_TEST_KILL_AT", point)]);
        assert_eq!(
            out1.status.signal(),
            Some(libc::SIGKILL),
            "[{point}] life 1 should die by SIGKILL at the kill point; stderr:\n{}",
            String::from_utf8_lossy(&out1.stderr)
        );

        // Life 2: no kill — restore from the mock store and finish the run.
        let out2 = run_agentd(&cfg, &[]);
        assert_eq!(
            out2.status.code(),
            Some(0),
            "[{point}] life 2 should restore and complete (exit 0); stderr:\n{}",
            String::from_utf8_lossy(&out2.stderr)
        );
        // The finish output prints exactly once — the run completed, not re-forked.
        assert_eq!(
            String::from_utf8_lossy(&out2.stdout).trim(),
            "done",
            "[{point}] life 2 emitted the finish output exactly once; stderr:\n{}",
            String::from_utf8_lossy(&out2.stderr)
        );

        std::fs::remove_file(&cfg).ok();
        drop(mock);
    }
}

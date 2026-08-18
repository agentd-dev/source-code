// SPDX-License-Identifier: AGPL-3.0-only
//! **Generations and the configuration digest** (RFC 0033 §3.2–§3.3) over the
//! real binary: a durable store that outlives several agentd lives (the mock MCP
//! server), and the two operator-facing behaviours the RFC exposes.
//!
//! * `--fresh` opens the NEXT generation without resuming — the counter is
//!   inherited (an operator can see which life they are in), the prior records
//!   are not adopted, and nothing is deleted.
//! * A settings edit that touches the state-shaping sections logs
//!   `store.config_changed` naming what moved, and the state is **still
//!   resumed**. Identity is `agent.name` (§3.1): keying it on a config hash
//!   would orphan a live workflow the first time somebody raised a limit, so the
//!   digest is a signal and never a gate.
#![cfg(all(unix, any(feature = "internal-mocks", debug_assertions)))]

mod common;

use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

/// A store-backed instance whose workflow is a deterministic `once → noop… →
/// finish` chain: no model, so the only variables are the generation and the
/// configuration. `extra` adds a step (the edit that must move the digest).
fn config(mock_uri: &str, extra_step: &str, finish_after: &str) -> String {
    let path = common::unique_path("generation-v2", "yaml");
    let yaml = format!(
        "config_version: \"2\"\n\
         agent:\n  name: generations\n\
         mcp:\n  servers:\n    - name: mock\n      endpoint: {mock_uri}\n\
         store:\n  kind: mcp\n  mcp:\n    server: mock\n\
         workflows:\n  - name: chain\n    steps:\n\
         \x20     s: {{kind: once}}\n\
         \x20     a: {{kind: noop, depends_on: [s]}}\n\
         {extra_step}\
         \x20     f: {{kind: finish, depends_on: [{finish_after}], status: completed, output: done}}\n\
         lifecycle:\n  run_until: idle\n  idle_grace: 1s\n\
         observability:\n  log_level: info\n"
    );
    std::fs::File::create(&path)
        .unwrap()
        .write_all(yaml.as_bytes())
        .unwrap();
    path
}

/// One agentd life. Returns its parsed stderr log lines (RFC 0010 §3.2 objects).
fn life(cfg: &str, extra_args: &[&str]) -> Vec<Value> {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .args(extra_args)
        .stdin(Stdio::null())
        .output()
        .expect("run agentd");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(0),
        "life {extra_args:?} should exit 0; stderr:\n{stderr}"
    );
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect()
}

fn event<'a>(lines: &'a [Value], name: &str) -> Option<&'a Value> {
    lines.iter().find(|l| l["event"] == name)
}

fn expect<'a>(lines: &'a [Value], name: &str) -> &'a Value {
    event(lines, name).unwrap_or_else(|| {
        panic!(
            "no {name} among: {:?}",
            lines.iter().map(|l| &l["event"]).collect::<Vec<_>>()
        )
    })
}

#[test]
fn fresh_starts_a_new_generation_and_does_not_resume() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let cfg = config(&mock.uri(), "", "a");

    // Life 1: an empty store — generation 1.
    assert_eq!(expect(&life(&cfg, &[]), "restore.fresh")["generation"], 1);

    // Life 2, ordinary: the run record life 1 left behind is resumed.
    let l2 = expect(&life(&cfg, &[]), "restore.done").clone();
    assert_eq!(l2["generation"], 2);
    assert_eq!(l2["entities"], 1, "an ordinary start resumes prior state");

    // Life 3, `--fresh`: the NEXT generation, resuming none of it. The counter
    // is inherited (not reset to 1), which is what makes "which life am I in?"
    // answerable at all, and the abandoned records are retired, not deleted.
    let l3 = life(&cfg, &["--fresh"]);
    assert!(
        event(&l3, "restore.done").is_none(),
        "--fresh must not resume: {l3:?}"
    );
    let fresh = expect(&l3, "restore.fresh");
    assert_eq!(fresh["generation"], 3);
    assert_eq!(fresh["superseded"], 2);
    assert!(
        fresh["retired"].as_u64().unwrap_or(0) >= 1,
        "the previous generation's records are retired, not deleted: {fresh}"
    );

    // Life 4, ordinary again: it adopts only what the FRESH generation wrote —
    // one run — never the retired records, which would undo the flag one boot
    // later. (Life 3 ran the chain again, so there is exactly one.)
    let l4 = expect(&life(&cfg, &[]), "restore.done").clone();
    assert_eq!(l4["generation"], 4);
    assert_eq!(l4["entities"], 1, "retired records stay retired: {l4}");

    std::fs::remove_file(&cfg).ok();
}

#[test]
fn an_edited_workflow_reports_config_changed_and_still_resumes() {
    let mock = common::spawn_mock_mcp("mock://noop", false);
    let cfg = config(&mock.uri(), "", "a");
    life(&cfg, &[]); // generation 1, writes the digest
    let before = expect(&life(&cfg, &[]), "restore.done").clone();
    assert_eq!(before["generation"], 2);
    std::fs::remove_file(&cfg).ok();

    // The most ordinary edit there is: one more step in the workflow.
    let edited = config(&mock.uri(), "      b: {kind: noop, depends_on: [a]}\n", "b");
    let lines = life(&edited, &[]);
    let changed = expect(&lines, "store.config_changed");
    assert_eq!(
        changed["level"], "warn",
        "it is the operator's cue: {changed}"
    );
    assert_eq!(
        changed["sections"],
        serde_json::json!(["workflows"]),
        "only the section that moved is named: {changed}"
    );
    assert_eq!(
        changed["msg"],
        "state was written under a different configuration — resuming anyway; --fresh to start a new generation"
    );
    // The whole point of §3.1: a signal, not a gate.
    let after = expect(&lines, "restore.done");
    assert_eq!(after["generation"], 3);
    assert_eq!(after["entities"], 1, "state is resumed anyway: {after}");

    // And the digest is re-recorded, so the same configuration is quiet next
    // time — a warning that never stops is a warning nobody reads.
    let again = life(&edited, &[]);
    assert!(
        event(&again, "store.config_changed").is_none(),
        "an unchanged configuration must not re-report: {again:?}"
    );
    assert_eq!(expect(&again, "restore.done")["generation"], 4);

    std::fs::remove_file(&edited).ok();
}

/// A flag that works but is undocumented is a bug (`--fresh` never reaches the
/// settings model, so it is not in the generated flag tables either).
#[test]
fn fresh_is_documented_in_help() {
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--help")
        .output()
        .expect("run agentd --help");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("--fresh"),
        "--help must list --fresh:\n{help}"
    );
    assert!(
        help.contains("start a NEW generation"),
        "--help must say what it does:\n{help}"
    );
}

// SPDX-License-Identifier: AGPL-3.0-only
//! **A step's `servers:` and a spawn's servers are a cap**, not only the set
//! a worker dials.
//!
//! The stream-taint check (RFC 0045 §5.11.3) and `docs/security.md` both read
//! an agent step's `servers:` as the servers it reaches. With
//! `security.policies` set, that stopped being true: every tool a rule might
//! touch is served by the supervisor, over the supervisor's own connections,
//! and the plan still offered every server's tools — so a step narrowed to `a`
//! could call `b` as soon as a rule covered `b.*`. A subagent's gated list had
//! the same shape: any tool a rule might touch, whatever servers the spawn
//! handed it. Each test runs a model that calls a `b` tool by name while held
//! to `a`, and reads `b`'s own log for whether the call reached it.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};

/// Run one config to idle with the mock model scripted to call `b.mock.ops`
/// and then echo what it got back; the daemon's stderr.
fn run(a: &common::MockMcp, b: &common::MockMcp, workflow: &str) -> String {
    let dir = common::unique_path("servers-cap", "d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        format!("{dir}/play.json"),
        serde_json::json!({"turns": [
            {"tool_calls": [{"name": "b.mock.ops", "arguments": {}}]},
            {"echo_tool_result": true}
        ]})
        .to_string(),
    )
    .unwrap();
    let cfg = format!(
        "agent: {{ name: cap, instruction: Do the work. }}\n\
         store: {{ kind: memory }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         intelligence: {{ endpoints: \"mock:file:{dir}/play.json\", model: mock }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2s }}\n\
         security:\n  policies:\n    - match: {{ tool: \"b.*\" }}\n      action: allow\n\
         mcp:\n  servers:\n\
         \x20   - {{ name: a, endpoint: \"{}\", ns: a }}\n\
         \x20   - {{ name: b, endpoint: \"{}\", ns: b }}\n\
         workflows:\n  - name: w\n    steps:\n      s: {{ kind: once }}\n{workflow}",
        a.uri(),
        b.uri(),
    );
    std::fs::write(format!("{dir}/c.yaml"), cfg).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &format!("{dir}/c.yaml")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run agentd");
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Whether a `tools/call` reached the mock.
fn called(m: &common::MockMcp) -> bool {
    m.log().lines().any(|l| l.starts_with("MOCK_CALL "))
}

#[test]
fn a_step_held_to_one_server_cannot_reach_another_through_a_policy() {
    let (a, b) = (
        common::spawn_mock_mcp("mock://a", false),
        common::spawn_mock_mcp("mock://b", false),
    );
    let log = run(
        &a,
        &b,
        "      ag: { kind: agent, depends_on: [s], instruction: go, servers: [a], tools: [\"a.*\", \"b.*\"] }\n\
         \x20     f: { kind: finish, depends_on: [ag], status: completed }\n",
    );
    assert!(
        log.contains("\"event\":\"run.done\""),
        "the run did not finish:\n{log}"
    );
    assert!(
        !called(&b),
        "a step held to `a` called `b`:\n{}\n{log}",
        b.log()
    );
}

#[test]
fn a_child_held_to_one_server_cannot_reach_another_through_a_policy() {
    let (a, b) = (
        common::spawn_mock_mcp("mock://a", false),
        common::spawn_mock_mcp("mock://b", false),
    );
    let log = run(
        &a,
        &b,
        "      c: { kind: subagent, depends_on: [s], instruction: go, servers: [a] }\n\
         \x20     f: { kind: finish, depends_on: [c], status: completed }\n",
    );
    assert!(
        log.contains("\"event\":\"run.done\""),
        "the run did not finish:\n{log}"
    );
    assert!(
        !called(&b),
        "a child held to `a` called `b`:\n{}\n{log}",
        b.log()
    );
}

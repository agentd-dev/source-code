// SPDX-License-Identifier: AGPL-3.0-only
//! **A gate may wait only for someone who can see it.**
//!
//! A human gate is a task, and a task is visible only to its owner and to
//! operators. A `to:` naming anyone else would open a gate its addressee gets
//! task-not-found for — before the addressee check ever runs — so it could
//! never be answered and would only ever time out. Every place a question is
//! addressed with `to:` refuses that, naming the principal:
//!
//! - `security.policies[].to` and a workflow `human` step, at load (exit 2);
//! - `workflow.create` and `workflow.update`, which reach the same validation;
//! - `ask_human`, when it is called — a model's `to`, or one a template
//!   rendered, is only known then.
//!
//! And the forms that do name an operator still load.
#![cfg(unix)]

mod common;

use std::process::{Command, Stdio};

/// Run the daemon on `cfg_text` (with `__DIR__` replaced by a scratch dir)
/// until it exits; `(exit code, stderr)`. Every config here ends idle within
/// seconds, so one that wrongly loads exits 0 rather than hanging.
fn run(args: &[&str], cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("gate-addressee", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__DIR__", &dir)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run agentd");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

const BASE: &str = "\
     agent: { name: gates }\n\
     store: { kind: memory }\n\
     observability: { log_level: info, log_content: true }\n\
     intelligence: { endpoints: \"mock:final\", model: mock }\n\
     lifecycle: { run_until: idle, idle_grace: 1s }\n";

/// The refusal the docs promise: exit 2, the place, the principal named, and
/// why it could never answer.
fn assert_refused(code: Option<i32>, log: &str, at: &str, named: &str) {
    assert_eq!(
        code,
        Some(2),
        "a gate its addressee cannot see must not load\n{log}"
    );
    assert!(
        log.contains(at) && log.contains(named) && log.contains("could never see the task"),
        "the refusal must name {at}, the principal {named:?} and why\n{log}"
    );
}

fn policy(to: &str) -> String {
    format!(
        "{BASE}security:\n  policies:\n    - {{match: {{tool: \"fs.*\"}}, action: ask, to: {to}}}\n"
    )
}

fn human_step(to: &str) -> String {
    format!(
        "{BASE}workflows:\n\
        \x20 - name: approve\n\
        \x20   steps:\n\
        \x20     s: {{kind: manual}}\n\
        \x20     g: {{kind: human, question: \"Refund?\", to: {to}, depends_on: [s]}}\n\
        \x20     f: {{kind: finish, depends_on: [g]}}\n"
    )
}

#[test]
fn a_policy_gate_addressed_to_anyone_but_an_operator_does_not_load() {
    let (code, log) = run(&[], &policy("\"user:alice\""));
    assert_refused(code, &log, "security.policies[0]", "user:alice");
    let (code, log) = run(&[], &policy("{role: user, labels: {team: finance}}"));
    assert_refused(code, &log, "security.policies[0]", "role user");
}

#[test]
fn a_workflow_human_gate_addressed_to_anyone_but_an_operator_does_not_load() {
    let (code, log) = run(&[], &human_step("\"*@finance.example\""));
    assert_refused(code, &log, "human.to", "*@finance.example");
    let (code, log) = run(&[], &human_step("{labels: {team: finance}}"));
    assert_refused(code, &log, "human.to", "team=finance");
}

/// The refusal is of who is named, not of addressing: every form that names
/// an operator still loads, beside a policy and a step that name nobody.
#[test]
fn a_gate_addressed_to_an_operator_still_loads() {
    for to in [
        "{role: operator}",
        "{role: operator, labels: {team: finance}}",
        "operator",
    ] {
        let (code, log) = run(&["--validate-config"], &policy(to));
        assert_eq!(code, Some(0), "policy to: {to} must load\n{log}");
        let (code, log) = run(&["--validate-config"], &human_step(to));
        assert_eq!(code, Some(0), "human to: {to} must load\n{log}");
    }
}

/// Drive one conversation through the mock model's `play` and return the
/// daemon's stderr. The playbook's last turn echoes the last tool result as
/// the reply, which `log_content` writes to the log — the tool result's own
/// text, as the model read it.
fn converse(play: &str) -> String {
    let cfg = "\
         agent: { name: gates, prompt: \"go\" }\n\
         store: { kind: memory }\n\
         observability: { log_level: info, log_content: true }\n\
         intelligence: { endpoints: \"mock:file:__DIR__/play.json\", model: mock }\n\
         lifecycle: { run_until: idle, idle_grace: 2s }\n";
    let dir = common::unique_path("gate-addressee-play", "d");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/play.json"), play).unwrap();
    std::fs::write(format!("{dir}/c.yaml"), cfg.replace("__DIR__", &dir)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &format!("{dir}/c.yaml")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run agentd");
    let _ = std::fs::remove_dir_all(&dir);
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// The `workflow.defined` lines of a log, which a refused definition never
/// writes.
fn defined(log: &str) -> Vec<&str> {
    log.lines()
        .filter(|l| l.contains("\"event\":\"workflow.defined\""))
        .collect()
}

fn definition(to: &str) -> String {
    format!(
        r#"{{"name": "approve", "steps": {{
             "s": {{"kind": "manual"}},
             "g": {{"kind": "human", "question": "Refund?", "to": {to}, "depends_on": ["s"]}},
             "f": {{"kind": "finish", "depends_on": ["g"]}}}}}}"#
    )
}

/// A workflow the model defines at runtime is held to the same rule as one
/// loaded from the config: a `workflow.create` of a gate addressed to a user
/// is refused, naming her, and defines nothing.
#[test]
fn workflow_create_refuses_a_gate_addressed_to_anyone_but_an_operator() {
    let log = converse(&format!(
        r#"{{"turns": [
             {{"tool_calls": [{{"name": "workflow.create", "arguments": {{"definition": {bad}}}}}]}},
             {{"echo_tool_result": true}}]}}"#,
        bad = definition(r#""user:alice""#),
    ));
    assert!(
        log.contains("`to` names user:alice, who could never see the task"),
        "workflow.create must refuse a gate addressed to user:alice, naming her\n{log}"
    );
    assert!(defined(&log).is_empty(), "nothing may be defined\n{log}");
}

/// …and so is a `workflow.update` that re-addresses a gate created addressed
/// to the operator — whose create succeeding is what makes the refusal
/// attributable to the addressee rather than to the definition's shape.
#[test]
fn workflow_update_refuses_a_gate_addressed_to_anyone_but_an_operator() {
    let log = converse(&format!(
        r#"{{"turns": [
             {{"tool_calls": [{{"name": "workflow.create", "arguments": {{"definition": {good}}}}}]}},
             {{"tool_calls": [{{"name": "workflow.update", "arguments": {{"name": "approve", "definition": {worse}}}}}]}},
             {{"echo_tool_result": true}}]}}"#,
        good = definition(r#"{"role": "operator"}"#),
        worse = definition(r#"{"role": "user"}"#),
    ));
    assert!(
        log.contains("`to` names role user, who could never see the task"),
        "workflow.update must refuse re-addressing the gate to a user\n{log}"
    );
    let defined = defined(&log);
    assert_eq!(defined.len(), 1, "only the create defines\n{log}");
    assert!(defined[0].contains("\"op\":\"workflow.create\""), "{log}");
}

/// `ask_human` is where a `to` nobody could check at load arrives — a model's,
/// or one a template rendered — so it is refused when called, naming the
/// principal, and no gate opens.
#[test]
fn ask_human_refuses_a_to_addressed_to_anyone_but_an_operator() {
    let log = converse(
        r#"{"turns": [
             {"tool_calls": [{"name": "ask_human", "arguments": {"question": "Ship it?", "to": "user:bob"}}]},
             {"echo_tool_result": true}]}"#,
    );
    assert!(
        log.contains("ask_human: `to` names user:bob, who could never see the task"),
        "ask_human must refuse a `to` naming user:bob, naming him\n{log}"
    );
    assert!(
        !log.contains("\"event\":\"human.ask\""),
        "and no gate may open for him\n{log}"
    );
}

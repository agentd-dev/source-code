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
        // The one secret a config here may reference: a principals rule's
        // bearer.
        .env("OPS", "gate-addressee-ops-bearer")
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
    converse_with("", play)
}

/// [`converse`] with `extra` appended to the config.
fn converse_with(extra: &str, play: &str) -> String {
    let cfg = format!(
        "\
         agent: {{ name: gates, prompt: \"go\" }}\n\
         store: {{ kind: memory }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         intelligence: {{ endpoints: \"mock:file:__DIR__/play.json\", model: mock }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2s }}\n{extra}"
    );
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

/// Every operator is addressed as `operator`, whichever `a2a.principals` rule
/// admitted it, so a rule's own id names no principal: `to: ops` is refused
/// with that reason, and the rule's labels are how a gate narrows to it.
#[cfg(feature = "a2a")]
#[test]
fn a_gate_addressed_to_an_operator_rules_id_is_refused_saying_how_to_name_it() {
    let principals = "a2a:\n  listen: http://127.0.0.1:1\n  principals:\n    - {id: ops, match: {bearer_ref: \"{{secret:OPS}}\"}, role: operator, labels: {team: sec}}\n";
    let with_ops = |cfg: String| format!("{cfg}{principals}");
    let (code, log) = run(&[], &with_ops(policy("ops")));
    assert_refused(code, &log, "security.policies[0]", "ops");
    assert!(
        log.contains("every operator is addressed as `operator`"),
        "the refusal says why a rule id is not an addressee\n{log}"
    );
    let (code, log) = run(
        &["--validate-config"],
        &with_ops(policy("{role: operator, labels: {team: sec}}")),
    );
    assert_eq!(code, Some(0), "the rule's labels name it\n{log}");
}

/// A `to` the step renders is only known when it runs, in any form `render`
/// rewrites, so the load leaves it to `ask_human` — which refuses the value
/// it renders to when that names someone who could never see the task, and
/// the step fails rather than opening a gate nobody can answer.
#[test]
fn a_templated_gate_addressee_is_held_to_the_rule_when_the_step_runs() {
    for to in [
        "\"{{ inputs.who }}\"",
        "{id: \"{{ inputs.who }}\"}",
        "{role: operator, id: \"{{ inputs.who }}\"}",
    ] {
        let (code, log) = run(&["--validate-config"], &human_step(to));
        assert_eq!(code, Some(0), "to: {to} is only known when it runs\n{log}");
    }
    let log = converse_with(
        "workflows:\n\
         \x20 - name: approve\n\
         \x20   steps:\n\
         \x20     s: {kind: manual}\n\
         \x20     g: {kind: human, question: \"Refund?\", to: \"{{ inputs.who }}\", depends_on: [s]}\n\
         \x20     f: {kind: finish, depends_on: [g]}\n",
        r#"{"turns": [
             {"tool_calls": [{"name": "workflow.run", "arguments": {"name": "approve", "inputs": {"who": "user:mallory"}, "wait": true, "timeout": "10s"}}]},
             {"echo_tool_result": true}]}"#,
    );
    assert!(
        log.contains("`to` names user:mallory, who could never see the task"),
        "the rendered addressee is refused, naming her\n{log}"
    );
    assert!(
        !log.contains("\"event\":\"human.ask\""),
        "and no gate opens for her\n{log}"
    );
}

/// A reload is held to the same rule as the load: a `to` naming a principal
/// who could never see the task is refused, naming them, and the running
/// configuration stays — a later valid reload changes only what it changes,
/// which it would not if the refused one had half-applied.
#[cfg(feature = "hot-reload")]
#[test]
fn a_reload_that_addresses_a_gate_to_anyone_but_an_operator_is_refused() {
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    let cfg_for = |to: &str, instruction: &str| {
        policy(to)
            .replace(
                "agent: { name: gates }",
                &format!("agent: {{ name: gates, instruction: {instruction} }}"),
            )
            .replace(
                "run_until: idle, idle_grace: 1s",
                "run_until: drained, drain_timeout: 5s",
            )
    };
    let dir = common::unique_path("gate-addressee-reload", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_for("operator", "first")).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agentd");
    let pid = child.id() as i32;
    let (tx, rx) = std::sync::mpsc::channel::<serde_json::Value>();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str(&line) {
                let _ = tx.send(v);
            }
        }
    });
    let wait = |name: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(v) if v["event"] == name => return v,
                Ok(v) if v["event"] == "config.reloaded" => panic!("reloaded: {v}"),
                Ok(_) => continue,
                Err(_) => panic!("timed out waiting for {name}"),
            }
        }
    };
    wait("proc.ready");

    std::fs::write(&cfg, cfg_for("\"user:alice\"", "first")).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let refused = wait("config.reload.invalid");
    let e = refused["error"].as_str().unwrap_or_default();
    assert!(
        e.contains("security.policies[0]")
            && e.contains("user:alice")
            && e.contains("could never see the task"),
        "the reload is refused, naming user:alice: {refused}"
    );
    assert!(child.try_wait().unwrap().is_none(), "still running");

    std::fs::write(&cfg, cfg_for("operator", "second")).unwrap();
    unsafe { libc::kill(pid, libc::SIGHUP) };
    let reloaded = loop {
        let v = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("config.reloaded");
        if v["event"] == "config.reloaded" {
            break v;
        }
    };
    let changed = reloaded["changed"].to_string();
    assert!(
        changed.contains("agent.instruction") && !changed.contains("security"),
        "the refused reload applied nothing, so only the instruction changed: {reloaded}"
    );

    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

// SPDX-License-Identifier: AGPL-3.0-only
//! A routing edge — a `switch` case, `on_error: goto`, a wait's `on_timeout`
//! — runs its target past its dependencies, never past its guard.
//!
//! A routed step used to be marked ready before its `when` was looked at, so
//! a CEL guard on a `switch` case, a recovery step or a timeout branch never
//! guarded anything. Now the edge skips only the dependency wait: a CEL
//! `when` is evaluated as for any step, and a decision guard is asked first
//! (a case not chosen has its guard pruned, unasked; a timeout branch's
//! guard waits for the timeout).
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Command, Stdio};

fn playbook(body: Value) -> String {
    let path = common::unique_path("routed-play", "json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// Run a job-shaped config; returns (exit code, stdout JSON, stderr log).
fn run(cfg_text: &str) -> (Option<i32>, Value, String) {
    let dir = common::unique_path("routed", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let stdout = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), stdout, log)
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// Whether `step` ever started.
fn started(log: &str, step: &str) -> bool {
    events(log, "step.start").iter().any(|e| e["step"] == step)
}

const BASE: &str = "agent: { name: r }\n\
     store: { kind: memory }\n\
     lifecycle: { run_until: idle, idle_grace: 400ms }\n\
     observability: { log_level: info, log_content: true }\n";

/// A `switch` case is held to its CEL `when`: a false one is pruned (and the
/// join after it goes on with the live branch), a true one runs.
#[cfg(feature = "cel")]
#[test]
fn a_switch_case_is_held_to_its_cel_when() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:  {{ kind: once }}\n\
         \x20     r1: {{ kind: switch, depends_on: [s], on: a, cases: {{ a: t1 }} }}\n\
         \x20     t1: {{ kind: assign, depends_on: [r1], value: \"t1 ran\", when: \"false\" }}\n\
         \x20     r2: {{ kind: switch, depends_on: [s], on: a, cases: {{ a: t2 }} }}\n\
         \x20     t2: {{ kind: assign, depends_on: [r2], value: \"t2 ran\", when: \"true\" }}\n\
         \x20     f:  {{ kind: finish, depends_on: [t1, t2], status: completed, output: {{ t1: \"{{{{ /steps/t1/status }}}}\", t2: \"{{{{ steps.t2.output }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["t1"], "pruned", "{out}\n{log}");
    assert_eq!(out["t2"], "t2 ran", "{out}");
    assert!(!started(&log, "t1"), "{log}");
}

/// An `on_error: goto` recovery step is held to its CEL `when`: one that
/// declines leaves the failure unrecovered, and the run fails with it.
#[cfg(feature = "cel")]
#[test]
fn a_recovery_step_that_declines_leaves_the_failure_standing() {
    let (code, _out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     bad: {{ kind: fail, depends_on: [s], message: \"boom\", on_error: \"goto:fix\" }}\n\
         \x20     fix: {{ kind: assign, depends_on: [bad], value: fixed, when: \"false\" }}\n\
         \x20     f:   {{ kind: finish, depends_on: [fix], status: completed, output: \"{{{{ steps.fix.output }}}}\" }}\n"
    ));
    assert_ne!(code, Some(0), "{log}");
    assert!(!started(&log, "fix"), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "failed", "{log}");
    assert!(
        done[0]["err"].as_str().is_some_and(|e| e.contains("boom")),
        "the failure that was not recovered is the one reported: {log}"
    );
}

/// A wait's `on_timeout` branch is held to its CEL `when`.
#[cfg(feature = "cel")]
#[test]
fn a_timeout_branch_is_held_to_its_cel_when() {
    let (code, _out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:      {{ kind: once }}\n\
         \x20     park:   {{ kind: wait, depends_on: [s], on: signal, signal: nobody-fires-this, timeout: 300ms, on_timeout: plan_b }}\n\
         \x20     ok:     {{ kind: assign, depends_on: [park], value: replied }}\n\
         \x20     f1:     {{ kind: finish, depends_on: [ok], status: completed }}\n\
         \x20     plan_b: {{ kind: assign, value: \"plan b\", when: \"false\" }}\n\
         \x20     f2:     {{ kind: finish, depends_on: [plan_b], status: completed }}\n"
    ));
    assert!(
        !events(&log, "step.timeout_routed").is_empty(),
        "the timeout took the routing edge:\n{log}"
    );
    assert!(!started(&log, "plan_b"), "{log}");
    assert_ne!(code, Some(0), "nothing finished the run: {log}");
}

/// A chat model `smart` on the top-level connection and a decision model
/// `jev` on a decisions provider, `roles.decide` on jev.
fn intelligence(chat: &str, decisions: &str) -> String {
    format!(
        "intelligence:\n  endpoints: \"mock:file:{chat}\"\n  providers:\n\
         \x20   jevp: {{ endpoints: \"mock:file:{decisions}\", dialect: decisions }}\n\
         \x20 models:\n\
         \x20   smart: {{ model: wire-smart }}\n\
         \x20   jev: {{ kind: decision, provider: jevp, model: typesafe/jev-1.13 }}\n\
         \x20 roles: {{ turn: smart, decide: jev }}\n"
    )
}

/// A `switch` case with a decision guard: the chosen case's guard is asked
/// and decides; the case not chosen is pruned with its guard unasked.
#[test]
fn a_switch_case_with_a_decision_guard_is_decided_and_the_other_case_is_not_asked() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    let dec = playbook(json!({"decisions": [
        {"when_contains": "Refunds are due.", "answers": {"holds": {"type": "noul", "noul": 0.9}}},
        {"when_contains": "Escalation is due.", "answers": {"holds": {"type": "noul", "noul": 0.1}}}
    ]}));
    for (chosen, statement, ran) in [
        ("refund", "Refunds are due.", true),
        ("refund", "Escalation is due.", false),
    ] {
        let (code, out, log) = run(&format!(
            "{BASE}{}workflows:\n  - name: w\n    steps:\n\
             \x20     s:      {{ kind: once }}\n\
             \x20     r:      {{ kind: switch, depends_on: [s], on: {chosen}, cases: {{ refund: refund, other: other }} }}\n\
             \x20     refund: {{ kind: assign, depends_on: [r], value: refunded, when: {{ noul: \"{statement}\" }} }}\n\
             \x20     other:  {{ kind: assign, depends_on: [r], value: other, when: {{ noul: \"Other is due.\" }} }}\n\
             \x20     f:      {{ kind: finish, depends_on: [refund, other], status: completed, output: {{ refund: \"{{{{ /steps/refund/status }}}}\", other: \"{{{{ /steps/other/status }}}}\", other_guard: \"{{{{ /steps/other.when/status }}}}\" }} }}\n",
            intelligence(&chat, &dec)
        ));
        if ran {
            assert_eq!(code, Some(0), "{log}");
            assert_eq!(out["refund"], "done", "{out}\n{log}");
        } else {
            // The chosen case declined and the other was not chosen: no
            // branch is live, so the run reaches no finish.
            assert!(!started(&log, "refund"), "{log}");
        }
        assert!(!started(&log, "other"), "{log}");
        assert!(
            !started(&log, "other.when"),
            "the case not chosen is not asked: {log}"
        );
        assert_eq!(events(&log, "intel.decision").len(), 1, "{log}");
    }
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// An `on_timeout` branch with a decision guard: the guard waits for the
/// timeout, as the branch does, and is asked only once it fires.
#[test]
fn a_timeout_branch_with_a_decision_guard_is_asked_after_the_timeout() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    let dec = playbook(json!({"decisions": [
        {"when_contains": "", "answers": {"holds": {"type": "noul", "noul": 0.9}}}
    ]}));
    let (code, _out, log) = run(&format!(
        "{BASE}{}workflows:\n  - name: w\n    steps:\n\
         \x20     s:      {{ kind: once }}\n\
         \x20     park:   {{ kind: wait, depends_on: [s], on: signal, signal: nobody-fires-this, timeout: 300ms, on_timeout: plan_b }}\n\
         \x20     ok:     {{ kind: assign, depends_on: [park], value: replied }}\n\
         \x20     f1:     {{ kind: finish, depends_on: [ok], status: completed }}\n\
         \x20     plan_b: {{ kind: assign, value: \"plan b\", when: {{ noul: \"A late answer still helps.\" }} }}\n\
         \x20     f2:     {{ kind: finish, depends_on: [plan_b], status: completed, output: \"{{{{ steps.plan_b.output }}}}\" }}\n",
        intelligence(&chat, &dec)
    ));
    assert_eq!(code, Some(0), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["output"], "plan b", "{log}");
    let lines: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let at = |pred: &dyn Fn(&Value) -> bool| lines.iter().position(pred);
    let routed = at(&|v| v["event"] == "step.timeout_routed").expect("routed");
    let asked = at(&|v| v["event"] == "step.start" && v["step"] == "plan_b.when").expect("asked");
    assert!(
        routed < asked,
        "the guard is asked after the timeout: {log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// Inside a body, a routed step is held to its CEL `when` too: skipped, as a
/// false guard skips a body step.
#[cfg(feature = "cel")]
#[test]
fn a_switch_case_in_a_body_is_held_to_its_cel_when() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ r: {{ kind: switch, on: a, cases: {{ a: t }} }}, t: {{ kind: assign, depends_on: [r], value: \"t ran\", when: \"false\" }}, u: {{ kind: assign, depends_on: [r], value: \"u ran\" }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ t: \"{{{{ /steps/sub.t/status }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["t"], "skipped", "{out}\n{log}");
    assert!(!started(&log, "sub.t"), "{log}");
}

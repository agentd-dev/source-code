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

/// Inside a body, a wait's `on_timeout` branch is parked until the timeout
/// routes to it — it used to run at the body's entry, and the timeout then
/// failed the body anyway. Now the wait's success path is pruned, the branch
/// runs after the timeout, and the body completes on it.
#[test]
fn a_timeout_branch_in_a_body_runs_on_the_timeout_and_the_body_completes() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ park: {{ kind: wait, on: signal, signal: nobody-fires-this, timeout: 300ms, on_timeout: plan_b }}, ok: {{ kind: assign, depends_on: [park], value: replied }}, plan_b: {{ kind: assign, value: \"plan b\" }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ ok: \"{{{{ /steps/sub.ok/status }}}}\", plan_b: \"{{{{ /steps/sub.plan_b/output }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["ok"], "pruned", "{out}\n{log}");
    assert_eq!(out["plan_b"], "plan b", "{out}");
    let lines: Vec<Value> = log
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    let at = |pred: &dyn Fn(&Value) -> bool| lines.iter().position(pred);
    let routed = at(&|v| v["event"] == "step.timeout_routed").expect("routed");
    let ran = at(&|v| v["event"] == "step.start" && v["step"] == "sub.plan_b").expect("ran");
    assert!(
        routed < ran,
        "the branch runs on the timeout, not at entry: {log}"
    );
}

/// …and when the wait is answered, the branch is pruned, never run.
#[cfg(feature = "cel")]
#[test]
fn a_timeout_branch_in_a_body_is_pruned_when_the_wait_is_answered() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ park: {{ kind: wait, on: condition, condition: \"true\", timeout: 5s, on_timeout: plan_b }}, ok: {{ kind: assign, depends_on: [park], value: replied }}, plan_b: {{ kind: assign, value: \"plan b\" }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ ok: \"{{{{ /steps/sub.ok/output }}}}\", plan_b: \"{{{{ /steps/sub.plan_b/status }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["ok"], "replied", "{out}\n{log}");
    assert_eq!(out["plan_b"], "pruned", "{out}");
    assert!(!started(&log, "sub.plan_b"), "{log}");
}

/// A recovery step in a body runs: the body used to fail before the
/// `on_error: goto` it had taken could run its target. Now the failure is
/// handled once the target has run, and the body completes on it.
#[test]
fn a_recovery_step_in_a_body_runs_and_the_body_completes() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ bad: {{ kind: fail, message: boom, on_error: \"goto:fix\" }}, after: {{ kind: assign, depends_on: [bad], value: after }}, fix: {{ kind: assign, depends_on: [bad], value: fixed }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ fix: \"{{{{ /steps/sub.fix/output }}}}\", after: \"{{{{ /steps/sub.after/status }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["fix"], "fixed", "{out}\n{log}");
    assert_eq!(
        out["after"], "pruned",
        "the success path is off the live path: {out}"
    );
}

/// …and one whose guard declines leaves the failure standing: the body fails
/// with it.
#[cfg(feature = "cel")]
#[test]
fn a_recovery_step_in_a_body_that_declines_leaves_the_failure_standing() {
    let (code, _out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ bad: {{ kind: fail, message: boom, on_error: \"goto:fix\" }}, fix: {{ kind: assign, depends_on: [bad], value: fixed, when: \"false\" }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed }}\n"
    ));
    assert_ne!(code, Some(0), "{log}");
    assert!(!started(&log, "sub.fix"), "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert!(
        done[0]["err"].as_str().is_some_and(|e| e.contains("boom")),
        "{log}"
    );
}

/// A `switch` in a body prunes its untaken case's whole tail, and the body
/// completes — a step depending only on the untaken case used to stay
/// pending, and the body never finished.
#[test]
fn an_untaken_case_in_a_body_prunes_its_tail_and_the_body_completes() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ r: {{ kind: switch, on: a, cases: {{ a: t, b: u }} }}, t: {{ kind: assign, depends_on: [r], value: \"t ran\" }}, u: {{ kind: assign, depends_on: [r], value: \"u ran\" }}, u2: {{ kind: assign, depends_on: [u], value: \"after u\" }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ t: \"{{{{ /steps/sub.t/output }}}}\", u2: \"{{{{ /steps/sub.u2/status }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["t"], "t ran", "{out}\n{log}");
    assert_eq!(out["u2"], "pruned", "{out}");
}

/// A chain of edges in a body — a timeout routed to a wait that times out
/// in turn — runs to its end, as at the top level: each link answers for
/// itself, and the body completes on the last.
#[test]
fn a_chain_of_timeouts_in_a_body_runs_to_its_end() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ ask: {{ kind: wait, on: signal, signal: nobody, timeout: 200ms, on_timeout: remind }}, remind: {{ kind: wait, on: signal, signal: nobody2, timeout: 200ms, on_timeout: escalate }}, escalate: {{ kind: assign, value: escalated }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ e: \"{{{{ /steps/sub.escalate/output }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["e"], "escalated", "{out}\n{log}");
}

/// A step that depends on a failure an edge took never runs — the failure's
/// success path is dead, as at the top level — even beside a live
/// dependency, and even while the recovery is still in flight.
#[test]
fn a_join_behind_a_routed_failure_in_a_body_never_runs() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ other: {{ kind: assign, value: o }}, bad: {{ kind: fail, message: boom, on_error: \"goto:fix\" }}, fix: {{ kind: sleep, depends_on: [bad], duration: 200ms }}, join: {{ kind: assign, depends_on: [bad, other], value: \"join ran\" }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ join: \"{{{{ /steps/sub.join/status }}}}\", fix: \"{{{{ /steps/sub.fix/status }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["join"], "pruned", "{out}\n{log}");
    assert_eq!(out["fix"], "done", "{out}");
    assert!(!started(&log, "sub.join"), "{log}");
}

/// A fallback that is both a wait's `on_timeout` target and another step's
/// `goto` target stays parked while either could still route to it: the
/// wait being answered does not prune it, the later failure runs it, and its
/// tail runs after it.
#[cfg(feature = "cel")]
#[test]
fn a_fallback_two_edges_share_waits_for_both_in_a_body() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ park: {{ kind: wait, on: condition, condition: \"true\", timeout: 5s, on_timeout: plan_b }}, slow: {{ kind: sleep, duration: 300ms }}, charge: {{ kind: fail, depends_on: [slow], message: declined, on_error: \"goto:plan_b\" }}, plan_b: {{ kind: assign, value: \"plan b\" }}, notify: {{ kind: assign, depends_on: [plan_b], value: notified }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ pb: \"{{{{ /steps/sub.plan_b/status }}}}\", n: \"{{{{ /steps/sub.notify/status }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["pb"], "done", "{out}\n{log}");
    assert_eq!(out["n"], "done", "{out}");
}

/// Each body step runs once: a step that finishes inline drives the body
/// again, and that pass ran the rest of the outer pass's ready list, which
/// then ran it a second time.
#[test]
fn each_body_step_runs_once() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:   {{ kind: once }}\n\
         \x20     sub: {{ kind: subgraph, depends_on: [s], body: {{ steps: {{ a: {{ kind: assign, value: a, writes: acc, mode: append }}, b: {{ kind: assign, value: b, writes: acc, mode: append }}, w: {{ kind: sleep, duration: 300ms }} }} }} }}\n\
         \x20     f:   {{ kind: finish, depends_on: [sub], status: completed, output: {{ acc: \"{{{{ vars.acc }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["acc"], json!(["a", "b"]), "{out}\n{log}");
    let starts = events(&log, "step.start");
    for step in ["sub.a", "sub.b", "sub.w"] {
        assert_eq!(
            starts.iter().filter(|e| e["step"] == step).count(),
            1,
            "{step}: {log}"
        );
    }
}

/// A wait's `on_timeout` target that the timeout never routes to is pruned
/// once the wait has ended, as a body prunes one — so a step depending on
/// both paths goes on with the live one. At the top level the target stayed
/// parked, and the run stalled when the wait succeeded.
#[cfg(feature = "cel")]
#[test]
fn an_unrouted_timeout_target_is_pruned_and_the_run_goes_on() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:    {{ kind: once }}\n\
         \x20     w:    {{ kind: wait, depends_on: [s], on: condition, condition: \"true\", timeout: 30s, on_timeout: late }}\n\
         \x20     late: {{ kind: assign, value: \"timed out\" }}\n\
         \x20     f:    {{ kind: finish, depends_on: [w, late], status: completed, output: {{ late: \"{{{{ /steps/late/status }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["late"], "pruned", "{out}\n{log}");
    assert!(!started(&log, "late"), "{log}");
}

/// A target a `switch` can still route to is not pruned when its wait ends:
/// the switch's case is an edge to it as much as the timeout is.
#[cfg(feature = "cel")]
#[test]
fn a_timeout_target_a_switch_can_still_route_to_waits_for_the_switch() {
    let (code, out, log) = run(&format!(
        "{BASE}workflows:\n  - name: w\n    steps:\n\
         \x20     s:    {{ kind: once }}\n\
         \x20     w:    {{ kind: wait, depends_on: [s], on: condition, condition: \"true\", timeout: 30s, on_timeout: page }}\n\
         \x20     r:    {{ kind: switch, depends_on: [w], on: urgent, cases: {{ urgent: page }} }}\n\
         \x20     page: {{ kind: assign, value: paged }}\n\
         \x20     fin:  {{ kind: finish, depends_on: [page], status: completed, output: {{ v: \"{{{{ steps.page.output }}}}\" }} }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["v"], "paged", "{out}\n{log}");
}

/// A gate the approval policy answers at once — `accept` takes the schema's
/// default — outputs it in the schema's shape, as a person's answer is.
#[test]
fn a_gate_accepted_by_policy_outputs_the_shaped_answer() {
    let (code, out, log) = run(&format!(
        "{}workflows:\n  - name: w\n    steps:\n\
         \x20     s:    {{ kind: once }}\n\
         \x20     gate: {{ kind: human, depends_on: [s], question: \"Ship it?\", schema: {{ type: object, properties: {{ approved: {{ type: boolean }} }}, default: {{ approved: true }} }} }}\n\
         \x20     fin:  {{ kind: finish, depends_on: [gate], status: completed, output: {{ v: \"approved={{{{ steps.gate.output.approved }}}}\" }} }}\n",
        BASE.replace("agent: { name: r }", "agent: { name: r, approval: accept }")
    ));
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["v"], "approved=true", "{out}\n{log}");
}

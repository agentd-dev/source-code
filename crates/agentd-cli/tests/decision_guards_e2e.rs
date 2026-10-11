// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0046 §7 (P5) — decision guards, end to end.
//!
//! A step's `when:` may be a decision: `{noul, state?, confidence?, model?}`.
//! It is asked as a `decide` step of its own, `<id>.when`, on the guard's
//! model or `roles.decide` — a decision model natively, else the turn model
//! through structured generation — and the step runs only when the statement
//! surely holds. Otherwise it is pruned, and so is the tail that depends on it
//! alone, while the rest of the run goes on. Doubt escalates where the model
//! names an `escalate` model and otherwise prunes; it never fails the run.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Command, Stdio};

fn playbook(body: Value) -> String {
    let path = common::unique_path("guards-play", "json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// Run a job-shaped config; returns (exit code, stdout JSON, stderr log).
fn run(cfg_text: &str) -> (Option<i32>, Value, String) {
    let dir = common::unique_path("guards", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
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

const BASE: &str = "agent: { name: g }\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info }\n\
     lifecycle: { run_until: idle, idle_grace: 2s }\n";

/// A chat model `smart` on the top-level connection (answering `chat`'s
/// playbook) and a decision model `jev` on a decisions provider (answering
/// `decisions`'), with `jev_extra` inside jev's entry and `roles` as given.
fn intelligence(chat: &str, decisions: &str, jev_extra: &str, roles: &str) -> String {
    format!(
        "intelligence:\n  endpoints: \"mock:file:{chat}\"\n  providers:\n\
         \x20   jevp: {{ endpoints: \"mock:file:{decisions}\", dialect: decisions }}\n\
         \x20 models:\n\
         \x20   smart: {{ model: wire-smart }}\n\
         \x20   jev: {{ kind: decision, provider: jevp, model: typesafe/jev-1.13{jev_extra} }}\n\
         \x20 roles: {roles}\n"
    )
}

/// A workflow whose `refund` step is guarded by `guard` and has a tail,
/// `after`, beside an independent branch, `other`. The job prints each
/// step's status and the guard's answer.
fn refund_job(guard: &str) -> String {
    r#"workflows:
  - name: w
    steps:
      s: { kind: once }
      triage: { kind: assign, depends_on: [s], value: "My card was charged twice; I want my money back." }
      refund: { kind: assign, depends_on: [triage], value: refunded, when: __GUARD__ }
      after: { kind: assign, depends_on: [refund], value: after }
      other: { kind: assign, depends_on: [s], value: "other ran" }
      f: { kind: finish, depends_on: [after, other], status: completed, output: { refund: "{{ /steps/refund/status }}", after: "{{ /steps/after/status }}", other: "{{ steps.other.output }}", guard: "{{ /steps/refund.when/output }}" } }
"#
    .replace("__GUARD__", guard)
}

const STATEMENT: &str = "{ noul: \"The customer asks for their money back.\" }";

/// A decisions playbook answering the guard's question with `noul` when the
/// request carries `needle`; anything else gets the mock's sure default.
fn holds_when(needle: &str, noul: f64) -> Value {
    json!({"decisions": [{"when_contains": needle, "answers": {"holds": {"type": "noul", "noul": noul}}}]})
}

#[test]
fn a_guard_that_holds_runs_its_step() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    // Matched on the dependency's output, which is the guard's default state.
    let dec = playbook(holds_when("charged twice", 0.8));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        refund_job(STATEMENT)
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["refund"], "done", "{out}\n{log}");
    assert_eq!(out["after"], "done", "{out}");
    assert_eq!(out["other"], "other ran", "{out}");
    assert_eq!(out["guard"]["holds"]["noul"], 0.8, "{out}");
    assert_eq!(out["guard"]["holds"]["calibrated"], true, "{out}");
    assert_eq!(out["guard"]["model"], "typesafe/jev-1.13", "{out}");
    // The guard is an ordinary step of the run, its decision logged.
    assert!(
        events(&log, "step.done")
            .iter()
            .any(|e| e["step"] == "refund.when" && e["status"] == "done"),
        "{log}"
    );
    assert_eq!(events(&log, "intel.decision").len(), 1, "{log}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

#[test]
fn a_guard_that_does_not_hold_prunes_its_step_and_tail_and_the_run_goes_on() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    let dec = playbook(holds_when("charged twice", 0.1));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        refund_job(STATEMENT)
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["refund"], "pruned", "{out}\n{log}");
    assert_eq!(out["after"], "pruned", "{out}");
    assert_eq!(out["other"], "other ran", "{out}");
    assert_eq!(out["guard"]["holds"]["noul"], 0.1, "{out}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// RFC 0046 §5.8: with no decision model configured, the turn model answers
/// the guard through structured generation — the answer's schema rides the
/// request as a native format — and the guard is honoured.
#[test]
fn with_no_decision_model_the_turn_model_answers_the_guard_through_structured_generation() {
    // Only a request that carries the format gets an answer that decides.
    let chat = playbook(json!({"turns": [{"content": "Probably, yes."}], "match": [{
        "when_contains": "\"response_format\"",
        "content": "{\"holds\": {\"noul\": 0.85}}"
    }]}));
    let dec = playbook(holds_when("charged twice", 0.1));
    let cfg = format!(
        "{BASE}{}{}",
        // No `decide` role: the guard runs on the turn's chat model.
        intelligence(&chat, &dec, "", "{ turn: smart }"),
        refund_job(STATEMENT)
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["refund"], "done", "{out}\n{log}");
    assert_eq!(out["guard"]["holds"]["noul"], 0.85, "{out}");
    assert_eq!(out["guard"]["holds"]["calibrated"], false, "{out}");
    assert_eq!(out["guard"]["model"], "wire-smart", "{out}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// Under the guard's `confidence`, an unsure answer with no model to
/// escalate to is handed on marked, and prunes the step — it does not fail
/// the run.
#[test]
fn an_unsure_guard_with_nowhere_to_escalate_prunes_its_step() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    // 0.6 is 0.2 from a coin flip, doubled: under 0.8.
    let dec = playbook(holds_when("charged twice", 0.6));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        refund_job("{ noul: \"The customer asks for their money back.\", confidence: 0.8 }")
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["guard"]["holds"]["unsure"], true, "{out}\n{log}");
    assert_eq!(out["refund"], "pruned", "{out}");
    assert_eq!(out["after"], "pruned", "{out}");
    assert_eq!(out["other"], "other ran", "{out}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// …and with an `escalate` model, the same doubt is asked again there, and
/// that answer is final.
#[test]
fn an_unsure_guard_is_escalated_and_the_escalate_models_answer_decides() {
    let chat = playbook(json!({"turns": [{"content": "{}"}], "match": [{
        "when_contains": "QUESTIONS:",
        "content": "{\"holds\": {\"noul\": 0.95}}"
    }]}));
    let dec = playbook(holds_when("charged twice", 0.6));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(
            &chat,
            &dec,
            ", escalate: smart",
            "{ turn: smart, decide: jev }"
        ),
        refund_job("{ noul: \"The customer asks for their money back.\", confidence: 0.8 }")
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["guard"]["escalated"], true, "{out}\n{log}");
    assert_eq!(out["guard"]["holds"]["noul"], 0.95, "{out}");
    assert_eq!(out["refund"], "done", "{out}");
    assert_eq!(events(&log, "intel.decision.escalate").len(), 1, "{log}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// A guard inside a body is asked per element and per scope: in a `foreach`
/// it decides about the element by default, in a `subgraph` about its
/// sibling dependencies' outputs; a guard that does not hold there skips its
/// step, as a false CEL guard does in a body.
#[test]
fn guards_in_bodies_are_asked_per_element_and_scope() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    let dec = playbook(json!({"decisions": [
        {"when_contains": "refund please", "answers": {"holds": {"type": "noul", "noul": 0.9}}},
        {"when_contains": "just a question", "answers": {"holds": {"type": "noul", "noul": 0.1}}},
        {"when_contains": "charged twice", "answers": {"holds": {"type": "noul", "noul": 0.2}}}
    ]}));
    let job = r#"workflows:
  - name: w
    steps:
      s: { kind: once }
      each: { kind: foreach, depends_on: [s], over: ["refund please", "just a question"], body: { steps: { x: { kind: assign, value: "{{ item }}", when: { noul: "The customer asks for money back." } } } } }
      sub: { kind: subgraph, depends_on: [s], body: { steps: { a: { kind: assign, value: "charged twice" }, b: { kind: assign, depends_on: [a], value: "b ran", when: { noul: "The customer asks for money back." } } } } }
      f: { kind: finish, depends_on: [each, sub], status: completed, output: { each: "{{ steps.each.output }}", b: "{{ /steps/sub.b/status }}", b_guard: "{{ /steps/sub.b.when/output/holds/noul }}" } }
"#;
    let cfg = format!(
        "{BASE}{}{job}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["each"], json!(["refund please", null]), "{out}\n{log}");
    assert_eq!(out["b"], "skipped", "{out}");
    assert_eq!(out["b_guard"], 0.2, "{out}");
    assert_eq!(events(&log, "intel.decision").len(), 3, "{log}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// A body step may be named `when`: inside a guarded `foreach` nested in a
/// subgraph, element 0's `when` step is `sub.each[0].when` in the run — the
/// body step, not the guard (whose record never carries an index). The
/// guard is asked once, and each element runs its own body.
#[test]
fn a_body_step_named_when_under_a_nested_guarded_step_is_the_body_step() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    let dec = playbook(json!({"decisions": [
        {"when_contains": "", "answers": {"holds": {"type": "noul", "noul": 0.9}}}
    ]}));
    let job = r#"workflows:
  - name: w
    steps:
      s: { kind: once }
      sub:
        kind: subgraph
        depends_on: [s]
        body:
          steps:
            each:
              kind: foreach
              over: ["a", "b"]
              when: { noul: "Worth doing." }
              body: { steps: { when: { kind: assign, value: "{{ item }}" } } }
      f: { kind: finish, depends_on: [sub], status: completed, output: { sub: "{{ steps.sub.output }}" } }
"#;
    let cfg = format!(
        "{BASE}{}{job}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["sub"], json!(["a", "b"]), "{out}\n{log}");
    assert_eq!(events(&log, "intel.decision").len(), 1, "{log}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// The guard's model is held to the deciding kinds' checks at load: a model
/// the catalogue does not declare is refused, naming the guard — for an
/// inline workflow (the config's own check) and for one read from a file
/// (the check every loaded workflow gets, nested bodies included).
#[test]
fn a_guard_on_an_undeclared_model_is_refused_at_load() {
    let chat = playbook(json!({"turns": [{"content": "not used"}]}));
    let dec = playbook(holds_when("charged twice", 0.8));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        refund_job("{ noul: \"The customer asks for their money back.\", model: nope }")
    );
    let (code, _, log) = run(&cfg);
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("step 'refund.when'") && log.contains("\"nope\" is not a declared model"),
        "{log}"
    );
    let wf = common::unique_path("guards-wf", "yaml");
    std::fs::write(
        &wf,
        "name: w\nsteps:\n  s: { kind: once }\n  sub:\n    kind: subgraph\n    depends_on: [s]\n    body: { steps: { b: { kind: noop, when: { noul: x, model: nope } } } }\n  f: { kind: finish, depends_on: [sub], status: completed }\n",
    )
    .unwrap();
    let cfg = format!(
        "{BASE}{}workflows:\n  - name: w\n    file: {wf}\n",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }")
    );
    let (code, _, log) = run(&cfg);
    assert_eq!(code, Some(2), "{log}");
    assert!(
        // The refusal is a JSON log line here, its quotes escaped.
        log.contains(r#"step \"b.when\": model \"nope\" is not a declared model"#),
        "{log}"
    );
    for f in [&chat, &dec, &wf] {
        let _ = std::fs::remove_file(f);
    }
}

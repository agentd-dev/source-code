// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0046 P3 — decision models in the loop, end to end.
//!
//! The goal check and the ask judge run on their roles' decision models:
//! the goal as two statements about the agent's status, a closed gate as a
//! choice among its answers, each acted on only above the role's
//! `confidence`. And no judge — decision model or chat — answers a gate on
//! the operator's behalf when the run asking carries outside input.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn playbook(body: Value) -> String {
    let path = common::unique_path("decisions-loop-play", "json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// A chat model on the top-level connection and a decision model `jev` on a
/// decisions provider, with `roles` as given.
fn intelligence(chat: &str, decisions: &str, roles: &str) -> String {
    format!(
        "intelligence:\n  endpoints: \"mock:file:{chat}\"\n  providers:\n\
         \x20   jevp: {{ endpoints: \"mock:file:{decisions}\", dialect: decisions }}\n\
         \x20 models:\n\
         \x20   smart: {{ model: wire-smart }}\n\
         \x20   jev: {{ kind: decision, provider: jevp, model: typesafe/jev-1.13 }}\n\
         \x20 roles: {roles}\n"
    )
}

/// Start agentd on `cfg`; wait until it exits or `done` holds over its log.
fn run_until(cfg: &str, secs: u64, done: impl Fn(&str) -> bool) -> (Option<i32>, String) {
    let dir = common::unique_path("decisions-loop", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/c.yaml");
    std::fs::write(&path, cfg).unwrap();
    let log_path = format!("{dir}/err.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &path])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(&log_path).unwrap()))
        .spawn()
        .expect("spawn agentd");
    let deadline = Instant::now() + Duration::from_secs(secs);
    let code = loop {
        if let Ok(Some(s)) = child.try_wait() {
            break s.code();
        }
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if done(&log) || Instant::now() > deadline {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    (code, log)
}

/// The goal check on a decision model: "achieved" is a statement about the
/// agent's status, true here well above the role's threshold, and the daemon
/// finishes.
#[test]
fn the_goal_check_runs_on_a_decision_model() {
    let chat = playbook(json!({"turns": [{"content": "x"}]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "achieved": {"type": "noul", "noul": 0.97},
        "stuck": {"type": "noul", "noul": 0.02}
    }}]}));
    let cfg = format!(
        "agent: {{ name: g, preflight: never }}\nstore: {{ kind: memory }}\n\
         observability: {{ log_level: info }}\n\
         lifecycle: {{ run_until: drained }}\n\
         goal:\n  statement: the inbox is empty\n  check: {{ every: 1s, via: agent }}\n  on_achieved: finish\n\
         {}workflows:\n  - name: idle\n    steps:\n      s: {{ kind: manual }}\n      f: {{ kind: finish, depends_on: [s] }}\n",
        intelligence(
            &chat,
            &dec,
            "{ turn: smart, goal: { model: jev, confidence: 0.6 } }"
        )
    );
    let (code, log) = run_until(&cfg, 20, |_| false);
    assert_eq!(code, Some(0), "the goal finished the daemon:\n{log}");
    let achieved = events(&log, "goal.achieved");
    assert!(
        achieved.iter().any(|e| e["via"] == "judge"
            && e["reason"]
                .as_str()
                .is_some_and(|r| r.contains("typesafe/jev-1.13"))),
        "{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// A gate whose answer is closed — an enum — is a choice for a decision
/// model standing in for the operator, acted on above the role's threshold.
#[test]
fn a_closed_gate_is_answered_by_a_decision_model_above_its_threshold() {
    let chat = playbook(json!({"turns": [{"content": "x"}]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "answer": {"type": "choice", "choice": "approve", "confidence": 0.93}
    }}]}));
    let cfg = format!(
        "agent: {{ name: a, ask_human_fallback: auto }}\nstore: {{ kind: memory }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2s }}\n\
         {}workflows:\n  - name: w\n    steps:\n\
         \x20     s: {{ kind: once }}\n\
         \x20     gate: {{ kind: human, depends_on: [s], question: \"Deploy the release?\", schema: {{ enum: [approve, deny] }}, timeout: 1s }}\n\
         \x20     f: {{ kind: finish, depends_on: [gate], status: completed, output: {{ got: \"{{{{steps.gate.output}}}}\" }} }}\n",
        intelligence(
            &chat,
            &dec,
            "{ turn: smart, approve: { model: jev, confidence: 0.8 } }"
        )
    );
    let (_, log) = run_until(&cfg, 20, |l| !events(l, "run.done").is_empty());
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "completed", "{log}");
    assert!(
        done[0]["output"].to_string().contains("approve"),
        "the decision model's choice answered the gate: {}\n{log}",
        done[0]
    );
    assert!(
        events(&log, "human.judge.start")
            .iter()
            .any(|e| e["decided"] == true),
        "{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// A decision model approving on the operator's behalf needs a threshold.
#[test]
fn a_decision_model_approver_without_a_threshold_is_refused_at_load() {
    let chat = playbook(json!({"turns": [{"content": "x"}]}));
    let dec = playbook(json!({"decisions": []}));
    let cfg = format!(
        "agent: {{ name: a }}\nstore: {{ kind: memory }}\n{}",
        intelligence(&chat, &dec, "{ turn: smart, approve: jev }")
    );
    let (code, log) = run_until(&cfg, 10, |_| false);
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("approves on the operator's behalf only above a threshold"),
        "{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// A run a webhook started carries what the caller sent; a gate it raises is
/// never answered on the operator's behalf, whatever the judge. (Webhooks are
/// served with the `a2a` feature.)
#[cfg(feature = "a2a")]
#[test]
fn no_judge_answers_a_gate_whose_run_carries_outside_input() {
    let chat = playbook(json!({"turns": [{"content": "approve"}]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "answer": {"type": "choice", "choice": "approve", "confidence": 0.99}
    }}]}));
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let cfg = format!(
        "agent: {{ name: h, ask_human_fallback: auto }}\nstore: {{ kind: memory }}\n\
         observability: {{ log_level: info }}\n\
         lifecycle: {{ run_until: drained }}\n\
         webhooks:\n  listen: http://127.0.0.1:{port}\n\
         {}workflows:\n  - name: hook\n    steps:\n\
         \x20     s: {{ kind: webhook, path: /deploy }}\n\
         \x20     gate: {{ kind: human, depends_on: [s], question: \"Deploy what the caller sent?\", schema: {{ enum: [approve, deny] }}, timeout: 1s }}\n\
         \x20     f: {{ kind: finish, depends_on: [gate], status: completed }}\n",
        intelligence(
            &chat,
            &dec,
            "{ turn: smart, approve: { model: jev, confidence: 0.5 } }"
        )
    );
    let poster = std::thread::spawn(move || {
        // Post once the listener is up.
        for _ in 0..200 {
            if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                use std::io::{Read, Write};
                let body = r#"{"release": "v9"}"#;
                let _ = write!(
                    s,
                    "POST /deploy HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let mut out = String::new();
                let _ = s.read_to_string(&mut out);
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
    let (_, log) = run_until(&cfg, 20, |l| !events(l, "run.done").is_empty());
    let _ = poster.join();
    let withheld = events(&log, "human.judge.withheld");
    assert_eq!(withheld.len(), 1, "{log}");
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "failed", "{log}");
    assert!(
        done[0]["err"]
            .as_str()
            .is_some_and(|e| e.contains("outside input")),
        "{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// Preflight on a decision model: the message's intent, plan and risk — and
/// which skills it calls for — are one decision; a plan it calls for is the
/// turn's to make, and the turn is told.
#[test]
fn preflight_triages_a_message_on_a_decision_model() {
    let chat = playbook(json!({"turns": [{"content": "done"}]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "intent": {"type": "choice", "choice": "task", "confidence": 0.9},
        "needs_plan": {"type": "noul", "noul": 0.92},
        "risk": {"type": "choice", "choice": "low", "confidence": 0.95}
    }}]}));
    let cfg = format!(
        "agent: {{ name: p, preflight: always, prompt: \"Migrate the billing tables to the new schema.\" }}\n\
         store: {{ kind: memory }}\nobservability: {{ log_level: info }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1s }}\n{}",
        intelligence(
            &chat,
            &dec,
            "{ turn: smart, preflight: { model: jev, confidence: 0.7 } }"
        )
    );
    let (_, log) = run_until(&cfg, 20, |l| {
        !events(l, "turn.done").is_empty() && !events(l, "preflight.verdict").is_empty()
    });
    let verdict = events(&log, "preflight.verdict");
    assert_eq!(verdict.len(), 1, "{log}");
    assert_eq!(verdict[0]["intent"], "task", "{log}");
    assert_eq!(verdict[0]["needs_plan"], true, "{log}");
    assert_eq!(verdict[0]["risk"], "low", "{log}");
    assert!(
        events(&log, "turn.model")
            .iter()
            .any(|e| e["role"] == "preflight" && e["model"] == "typesafe/jev-1.13"),
        "{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// The shape a gate's answer is usually written in — one property, here a
/// `decision` enum — is a closed ask too, and the choice is shaped back into
/// it; the judge's decision is on the record.
#[test]
fn a_gate_whose_answer_is_one_enum_property_is_answered_in_that_shape() {
    let chat = playbook(json!({"turns": [{"content": "x"}]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "answer": {"type": "choice", "choice": "hold", "confidence": 0.91}
    }}]}));
    let cfg = format!(
        "agent: {{ name: a, ask_human_fallback: auto }}\nstore: {{ kind: memory }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2s }}\n\
         {}workflows:\n  - name: w\n    steps:\n\
         \x20     s: {{ kind: once }}\n\
         \x20     gate: {{ kind: human, depends_on: [s], question: \"File the return now?\", schema: {{ type: object, required: [decision], properties: {{ decision: {{ enum: [file, hold] }} }} }}, timeout: 1s }}\n\
         \x20     f: {{ kind: finish, depends_on: [gate], status: completed, output: \"{{{{steps.gate.output}}}}\" }}\n",
        intelligence(
            &chat,
            &dec,
            "{ turn: smart, approve: { model: jev, confidence: 0.8 } }"
        )
    );
    let (_, log) = run_until(&cfg, 20, |l| !events(l, "run.done").is_empty());
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "completed", "{log}");
    assert!(
        done[0]["output"].to_string().contains("hold"),
        "{}\n{log}",
        done[0]
    );
    let decided = events(&log, "intel.decision");
    assert!(
        decided.iter().any(|d| d["role"] == "approve"
            && d["answer"] == "hold"
            && d["model"] == "typesafe/jev-1.13"),
        "{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// An approval is never acted on below the threshold — an escalated answer
/// included: still unsure, it stays undecided and the gate fails.
#[test]
fn an_escalated_approval_is_held_to_the_threshold_too() {
    let chat = playbook(json!({"turns": [{"content": "x"}], "match": [{
        "when_contains": "QUESTIONS:",
        "content": "{\"answer\": {\"choice\": \"approve\", \"confidence\": 0.3}}"
    }]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "answer": {"type": "choice", "choice": "approve", "confidence": 0.3}
    }}]}));
    let cfg = format!(
        "agent: {{ name: a, ask_human_fallback: auto }}\nstore: {{ kind: memory }}\n\
         observability: {{ log_level: info }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 2s }}\n\
         intelligence:\n  endpoints: \"mock:file:{chat}\"\n  providers:\n\
         \x20   jevp: {{ endpoints: \"mock:file:{dec}\", dialect: decisions }}\n\
         \x20 models:\n\
         \x20   smart: {{ model: wire-smart }}\n\
         \x20   jev: {{ kind: decision, provider: jevp, model: typesafe/jev-1.13, escalate: smart }}\n\
         \x20 roles: {{ turn: smart, approve: {{ model: jev, confidence: 0.8 }} }}\n\
         workflows:\n  - name: w\n    steps:\n\
         \x20     s: {{ kind: once }}\n\
         \x20     gate: {{ kind: human, depends_on: [s], question: \"Delete the production database?\", schema: {{ enum: [approve, deny] }}, timeout: 1s }}\n\
         \x20     f: {{ kind: finish, depends_on: [gate], status: completed }}\n"
    );
    let (_, log) = run_until(&cfg, 20, |l| !events(l, "run.done").is_empty());
    let done = events(&log, "run.done");
    assert_eq!(done.len(), 1, "{log}");
    assert_eq!(done[0]["status"], "failed", "{log}");
    assert!(
        events(&log, "intel.decision")
            .iter()
            .any(|d| d["role"] == "approve"
                && d["answer"] == "UNDECIDED"
                && d["escalated"] == true),
        "{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

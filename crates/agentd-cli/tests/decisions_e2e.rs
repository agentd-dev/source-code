// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0046 P2 — decisions, end to end.
//!
//! A decision model on a `dialect: decisions` provider answers a `decide`
//! node, and `classify`/`route`/`judge`, natively; a chat model answers the
//! same decision by emulation, uncalibrated. An answer under the node's
//! `confidence` is asked again on the model's `escalate` model, fails the
//! node, or is handed on marked — never quietly acted on. The agent reaches
//! the same contract through the `decide` tool.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Command, Stdio};

fn playbook(body: Value) -> String {
    let path = common::unique_path("decisions-play", "json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

/// Run a job-shaped config; returns (exit code, stdout JSON, stderr log).
fn run(cfg_text: &str) -> (Option<i32>, Value, String) {
    let dir = common::unique_path("decisions", "d");
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

const BASE: &str = "agent: { name: d }\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info }\n\
     lifecycle: { run_until: idle, idle_grace: 2s }\n";

/// The intelligence section: a chat model on the top-level connection
/// (answering `chat`'s playbook), a decision model `jev` on a decisions
/// provider (answering `decisions`'), `roles.decide: jev` unless `roles` says
/// otherwise; `jev_extra` lands inside jev's model entry.
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

/// A workflow with one deciding step `d` (its fields given) whose output the
/// job prints.
fn job(step: &str) -> String {
    format!(
        "workflows:\n\
         \x20 - name: w\n    steps:\n\
         \x20     s: {{ kind: once }}\n\
         \x20     d: {{ depends_on: [s], {step} }}\n\
         \x20     f: {{ kind: finish, depends_on: [d], status: completed, output: {{ out: \"{{{{steps.d.output}}}}\", err: \"{{{{steps.d.error | ok}}}}\" }} }}\n"
    )
}

fn questions() -> &'static str {
    "state: \"My card was charged twice and nothing works.\", questions: { queue: { choice: { billing: \"payment problems\", tech: \"bugs and outages\" } }, urgent: { noul: \"The customer cannot work at all.\" } }"
}

fn native_answers(queue_confidence: f64) -> Value {
    json!({"decisions": [{"answers": {
        "queue": {"type": "choice", "choice": "tech", "probabilities": {"billing": 0.2, "tech": 0.8}, "confidence": queue_confidence},
        "urgent": {"type": "noul", "noul": 0.71}
    }, "usage": {"input_tokens": 120, "output_tokens": 9}}]})
}

#[test]
fn a_decide_node_on_a_decision_model_is_answered_natively() {
    let (chat, dec) = (
        playbook(json!({"turns": [{"content": "not used"}]})),
        playbook(native_answers(0.84)),
    );
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        job(&format!("kind: decide, {}", questions()))
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let d = &out["out"];
    assert_eq!(d["queue"]["choice"], "tech", "{out}\n{log}");
    assert_eq!(d["queue"]["probabilities"]["tech"], 0.8);
    assert_eq!(d["queue"]["calibrated"], true);
    assert_eq!(d["urgent"]["noul"], 0.71);
    assert_eq!(d["model"], "typesafe/jev-1.13");
    assert_eq!(d["escalated"], false);
    let decided = events(&log, "intel.decision");
    assert_eq!(decided.len(), 1, "{log}");
    assert_eq!(decided[0]["answers"]["queue"]["choice"], "tech");
    // The state is content, and is not on the log line.
    assert!(!decided[0].to_string().contains("charged twice"), "{log}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

#[test]
fn a_decide_node_on_a_chat_model_is_emulated_and_uncalibrated() {
    let chat = playbook(json!({"turns": [{"content": "{}"}], "match": [{
        "when_contains": "QUESTIONS:",
        "content": "{\"queue\": {\"choice\": \"billing\", \"confidence\": 0.7}, \"urgent\": {\"noul\": 0.2}}"
    }]}));
    let dec = playbook(native_answers(0.9));
    let cfg = format!(
        "{BASE}{}{}",
        // `decide` unbound: the decision runs on the turn's chat model.
        intelligence(&chat, &dec, "", "{ turn: smart }"),
        job(&format!("kind: decide, {}", questions()))
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let d = &out["out"];
    assert_eq!(d["queue"]["choice"], "billing", "{out}\n{log}");
    assert_eq!(d["queue"]["calibrated"], false);
    assert_eq!(d["urgent"]["noul"], 0.2);
    assert_eq!(d["model"], "wire-smart");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// `classify` keeps its output on a decision model: `{class, confidence}`.
#[test]
fn classify_on_a_decision_model_keeps_its_output() {
    let chat = playbook(json!({"turns": [{"content": "{\"class\": \"from-chat\"}"}]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "class": {"type": "choice", "choice": "billing", "confidence": 0.88}
    }}]}));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        job(
            "kind: classify, input: \"charged twice\", classes: { billing: \"payment problems\", tech: \"bugs\" }"
        )
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["out"]["class"], "billing", "{out}\n{log}");
    assert_eq!(out["out"]["confidence"], 0.88);
    assert_eq!(out["out"]["calibrated"], true);
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// An answer under the node's threshold is asked again on the decision
/// model's `escalate` model — here a chat model, which emulates — and that
/// answer is final.
#[test]
fn an_unsure_answer_is_escalated_once() {
    let chat = playbook(json!({"turns": [{"content": "{}"}], "match": [{
        "when_contains": "QUESTIONS:",
        "content": "{\"queue\": {\"choice\": \"billing\", \"confidence\": 0.3}, \"urgent\": {\"noul\": 0.95}}"
    }]}));
    let dec = playbook(native_answers(0.4));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(
            &chat,
            &dec,
            ", escalate: smart",
            "{ turn: smart, decide: jev }"
        ),
        job(&format!("kind: decide, confidence: 0.8, {}", questions()))
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let d = &out["out"];
    assert_eq!(
        d["queue"]["choice"], "billing",
        "the escalate model's answer: {out}\n{log}"
    );
    assert_eq!(d["escalated"], true);
    assert_eq!(d["model"], "wire-smart");
    // Final, though its own confidence is lower still: one hop.
    assert!(d["queue"].get("unsure").is_none(), "{out}");
    let esc = events(&log, "intel.decision.escalate");
    assert_eq!(esc.len(), 1, "{log}");
    assert_eq!(esc[0]["to"], "wire-smart");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// With no model to escalate to, an unsure answer fails the node — a
/// threshold the author set is never quietly ignored — and `continue` hands
/// it on marked instead.
#[test]
fn an_unsure_answer_fails_the_node_or_is_marked() {
    let chat = playbook(json!({"turns": [{"content": "{}"}]}));
    let dec = playbook(native_answers(0.4));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        job(&format!(
            "kind: decide, confidence: 0.8, on_error: continue, {}",
            questions()
        ))
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let err = out["err"].as_str().unwrap_or_default();
    assert!(err.contains("unsure: queue"), "{out}\n{log}");

    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        job(&format!(
            "kind: decide, confidence: 0.8, on_unsure: continue, {}",
            questions()
        ))
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["out"]["queue"]["unsure"], true, "{out}\n{log}");
    // The noul is sure enough (0.71 → 0.42 under 0.8, so it is unsure too).
    assert_eq!(out["out"]["urgent"]["unsure"], true, "{out}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// A `decide` (or `think`) request is answered once. The answer used to
/// leave the request pending, so the worker's exit answered it again with
/// "think worker exited without a result" — and a requester that read both
/// replies could take the error (a CI-only failure: it depends on timing).
/// The double reply itself is not timing-dependent, so it is what this
/// counts, in the debug log's `tool.reply` lines.
#[test]
fn a_decide_request_is_answered_once() {
    let chat = playbook(json!({"turns": [
        {"tool_calls": [{"name": "decide", "arguments": {
            "state": "My card was charged twice.",
            "questions": {"queue": {"choice": ["billing", "tech"]}}
        }}]},
        {"echo_tool_result": true}
    ]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "queue": {"type": "choice", "choice": "billing", "confidence": 0.93}
    }}]}));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        job("kind: agent, instruction: \"triage the ticket\"")
    )
    .replace("log_level: info", "log_level: debug");
    let (code, _out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let request = events(&log, "tool.request")
        .into_iter()
        .find(|e| e["tool"] == "decide")
        .expect("the decide request");
    let replies: Vec<Value> = events(&log, "tool.reply")
        .into_iter()
        .filter(|e| e["node"] == request["node"] && e["req"] == request["req"])
        .collect();
    assert_eq!(replies.len(), 1, "{replies:?}\n{log}");
    assert_eq!(replies[0]["is_error"], false, "{log}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// The agent reaches the contract through the `decide` tool, on
/// `roles.decide`.
#[test]
fn an_agent_decides_with_the_decide_tool() {
    let chat = playbook(json!({"turns": [
        {"tool_calls": [{"name": "decide", "arguments": {
            "state": "My card was charged twice.",
            "questions": {"queue": {"choice": ["billing", "tech"]}}
        }}]},
        {"echo_tool_result": true}
    ]}));
    let dec = playbook(json!({"decisions": [{"answers": {
        "queue": {"type": "choice", "choice": "billing", "confidence": 0.93}
    }}]}));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart, decide: jev }"),
        job("kind: agent, instruction: \"triage the ticket\"")
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let text = out["out"].to_string();
    assert!(
        text.contains("billing") && text.contains("typesafe/jev-1.13"),
        "the tool's answer reached the agent: {out}\n{log}"
    );
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// Kinds are checked at load: a decision model writes nothing, and a chat
/// model is never reached on a decisions endpoint.
#[test]
fn a_decision_model_where_a_model_must_write_is_refused_at_load() {
    let chat = playbook(json!({"turns": [{"content": "x"}]}));
    let dec = playbook(native_answers(0.9));
    for (roles, step, why) in [
        (
            "{ turn: jev }",
            "kind: decide, state: x, questions: { q: { noul: y } }",
            "intelligence.roles.turn: \"jev\" is a decision model",
        ),
        (
            "{ turn: smart }",
            "kind: think, prompt: x, model: jev",
            "a `think` node writes",
        ),
        (
            "{ turn: smart }",
            "kind: decide, confidence: 0.5, on_unsure: escalate, model: jev, state: x, questions: { q: { noul: y } }",
            "names no `escalate` model",
        ),
    ] {
        let cfg = format!(
            "{BASE}{}{}",
            intelligence(&chat, &dec, "", roles),
            job(step)
        );
        let (code, _, log) = run(&cfg);
        assert_eq!(code, Some(2), "{roles} / {step}\n{log}");
        assert!(log.contains(why), "{why}\n{log}");
    }
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// A chat model that misses the decision's shape is told why and asked once
/// more; its second answer is the decision.
#[test]
fn a_malformed_emulated_answer_is_asked_again_once() {
    let chat = playbook(json!({"turns": [{"content": "{}"}], "match": [
        {"when_contains": "cannot be used",
         "content": "{\"queue\": {\"choice\": \"tech\", \"confidence\": 0.9}, \"urgent\": {\"noul\": 0.9}}"},
        {"when_contains": "QUESTIONS:", "content": "I think it is a billing issue."}
    ]}));
    let dec = playbook(native_answers(0.9));
    let cfg = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart }"),
        job(&format!("kind: decide, {}", questions()))
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    assert_eq!(out["out"]["queue"]["choice"], "tech", "{out}\n{log}");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// Kinds are checked for a workflow however it arrives — here from a file,
/// with the decision model inside a loop's body.
#[test]
fn a_decision_model_in_a_file_workflow_is_refused_at_load() {
    let chat = playbook(json!({"turns": [{"content": "x"}]}));
    let dec = playbook(native_answers(0.9));
    let wf = common::unique_path("decisions-wf", "yaml");
    std::fs::write(
        &wf,
        "name: w\nsteps:\n  s: { kind: once }\n  each:\n    kind: foreach\n    depends_on: [s]\n    over: [1, 2]\n    batch: { size: 1, parallel: 1 }\n    body: { steps: { t: { kind: think, prompt: x, model: jev } } }\n  f: { kind: finish, depends_on: [each], status: completed }\n",
    )
    .unwrap();
    let cfg = format!(
        "{BASE}{}workflows:\n  - name: w\n    file: {wf}\n",
        intelligence(&chat, &dec, "", "{ turn: smart }")
    );
    let (code, _, log) = run(&cfg);
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("a `think` node writes"), "{log}");
    for f in [&chat, &dec, &wf] {
        let _ = std::fs::remove_file(f);
    }
}

/// RFC 0046 §5.8: a decide node with no decision model configured is
/// answered by the turn model through structured generation — the answer's
/// schema rides the request as a native format (`response_format`, strict),
/// so a model that only answers when held to it answers, and is read.
#[test]
fn a_decide_node_with_no_decision_model_is_answered_through_structured_generation() {
    // Only a request that carries the format gets a usable answer: without
    // it the model rambles, and the node fails.
    let chat = playbook(
        json!({"turns": [{"content": "Probably billing, I would say."}], "match": [{
            "when_contains": "\"response_format\"",
            "content": "{\"queue\": {\"choice\": \"billing\", \"confidence\": 0.8}, \"urgent\": {\"noul\": 0.3}}"
        }]}),
    );
    let dec = playbook(native_answers(0.9));
    let cfg = format!(
        "{BASE}{}{}",
        // No `decide` role: the decision runs on the turn's chat model.
        intelligence(&chat, &dec, "", "{ turn: smart }"),
        job(&format!("kind: decide, {}", questions()))
    );
    let (code, out, log) = run(&cfg);
    assert_eq!(code, Some(0), "{log}");
    let d = &out["out"];
    assert_eq!(d["queue"]["choice"], "billing", "{out}\n{log}");
    assert_eq!(d["queue"]["calibrated"], false);
    assert_eq!(d["urgent"]["noul"], 0.3);
    assert_eq!(d["model"], "wire-smart");
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// `structured_output: prompt` sends no format — on the top-level
/// connection, and on a provider's — for a server that refuses one; the
/// answer is asked in the prompt and still read.
#[test]
fn a_connection_asked_in_the_prompt_alone_is_sent_no_format() {
    // A request that carries a format gets an answer the node cannot use;
    // only the prompt-only request is answered.
    let chat = playbook(json!({"turns": [{"content": "{}"}], "match": [
        {"when_contains": "\"response_format\"", "content": "a format reached a server that refuses one"},
        {"when_contains": "QUESTIONS:",
         "content": "{\"queue\": {\"choice\": \"tech\", \"confidence\": 0.9}, \"urgent\": {\"noul\": 0.6}}"}
    ]}));
    let dec = playbook(native_answers(0.9));
    let top = format!(
        "{BASE}{}{}",
        intelligence(&chat, &dec, "", "{ turn: smart }").replacen(
            "intelligence:\n",
            "intelligence:\n  structured_output: prompt\n",
            1
        ),
        job(&format!("kind: decide, {}", questions()))
    );
    // The turn model on a provider of its own, asked in the prompt alone
    // while the top-level connection keeps `auto`.
    let provider = format!(
        "{BASE}intelligence:\n  endpoints: \"mock:file:{chat}\"\n  providers:\n\
         \x20   plain: {{ endpoints: \"mock:file:{chat}\", structured_output: prompt }}\n\
         \x20 models:\n\
         \x20   smart: {{ model: wire-smart, provider: plain }}\n\
         \x20 roles: {{ turn: smart }}\n{}",
        job(&format!("kind: decide, {}", questions()))
    );
    for cfg in [top, provider] {
        let (code, out, log) = run(&cfg);
        assert_eq!(code, Some(0), "{cfg}\n{log}");
        assert_eq!(out["out"]["queue"]["choice"], "tech", "{out}\n{log}");
    }
    for f in [&chat, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

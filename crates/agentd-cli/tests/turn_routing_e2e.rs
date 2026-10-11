// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0046 P5 — per-turn routing, end to end.
//!
//! `intelligence.routes` names the models a turn may run on, each with when
//! to use it. Preflight chooses one for the message — a decision model as one
//! more question, else the turn model through structured generation — and
//! the turn that answers the message runs on it. The mock answers each turn
//! by the wire model in its request body, so every test reads in the REPLY
//! which model answered, not only what the log says was chosen.
#![cfg(unix)]

mod common;

use serde_json::{Value, json};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn playbook(body: Value) -> String {
    let path = common::unique_path("routing-play", "json");
    std::fs::write(&path, body.to_string()).unwrap();
    path
}

fn events(log: &str, name: &str) -> Vec<Value> {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

/// The schema's `route` enum, as a native format carries it.
const ENUM: &str = r#""route":{"enum":["deep","fast"]}"#;
/// The routes as preflight's prompt lists them, with their criteria.
const LISTED: &str = "- deep: multi-step work or careful analysis";

/// The chat connection's playbook: preflight answers `preflight` when its
/// request carries `needle` — the routes, in its schema or its prompt — and
/// each turn answers with the model it was sent to.
fn chat(needle: &str, preflight: Value) -> String {
    playbook(json!({
        "match": [
            {"when_contains": needle, "content": preflight},
            {"when_contains": "\"model\":\"d-1\"", "content": "answered on deep"},
            {"when_contains": "\"model\":\"f-1\"", "content": "answered on fast"}
        ],
        "turns": [{"content": "answered on the turn model"}]
    }))
}

/// One message (`agent.prompt`) to an agent whose turns run on `smart`
/// unless routed to `fast` or `deep`. `decisions` adds a decision model
/// `jev` on a decisions provider; `roles` binds the roles; `connection` is
/// more of the top-level connection's settings.
fn config(
    prompt: &str,
    preflight: &str,
    chat: &str,
    decisions: Option<&str>,
    roles: &str,
    connection: &str,
) -> String {
    let (provider, jev) = match decisions {
        Some(d) => (
            format!(
                "  providers:\n    jevp: {{ endpoints: \"mock:file:{d}\", dialect: decisions }}\n"
            ),
            "    jev: { kind: decision, provider: jevp, model: typesafe/jev-1.13 }\n".to_string(),
        ),
        None => (String::new(), String::new()),
    };
    format!(
        "agent: {{ name: r, preflight: {preflight}, prompt: \"{prompt}\" }}\n\
         store: {{ kind: memory }}\n\
         observability: {{ log_level: info, log_content: true }}\n\
         lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         intelligence:\n  endpoints: \"mock:file:{chat}\"\n{connection}{provider}\
         \x20 models:\n\
         \x20   smart: {{ model: wire-smart }}\n\
         \x20   fast: {{ model: f-1 }}\n\
         \x20   deep: {{ kind: reasoning, model: d-1 }}\n{jev}\
         \x20 roles: {roles}\n\
         \x20 routes:\n\
         \x20   fast: \"a quick answer or a status question\"\n\
         \x20   deep: \"multi-step work or careful analysis\"\n"
    )
}

/// Start agentd on `cfg` and wait until it exits (or 30 s pass).
fn run(cfg: &str) -> String {
    let dir = common::unique_path("routing", "d");
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
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        if Instant::now() > deadline {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    log
}

/// What the message's turn answered, and the wire model it was sent to.
fn answered(log: &str) -> (Value, Vec<Value>) {
    let reply = events(log, "turn.reply")
        .first()
        .map(|r| r["text"].clone())
        .unwrap_or(Value::Null);
    let turns = events(log, "turn.model")
        .into_iter()
        .filter(|m| m["role"] == "turn")
        .map(|m| m["model"].clone())
        .collect();
    (reply, turns)
}

const WORK: &str = "Analyse last quarter's churn and propose fixes.";

/// With no decision model configured, preflight runs on the turn model
/// through structured generation: its schema's `route` enum and its prompt
/// name the routes, and the turn runs on the one it chose.
#[test]
fn a_chat_preflight_routes_the_turn_to_the_model_it_chose() {
    let c = chat(
        ENUM,
        json!({"intent": "task", "needs_plan": false, "risk": "low", "route": "deep"}),
    );
    let log = run(&config(WORK, "always", &c, None, "{ turn: smart }", ""));
    let verdict = events(&log, "preflight.verdict");
    assert_eq!(verdict.len(), 1, "{log}");
    assert_eq!(verdict[0]["route"], "deep", "{log}");
    assert!(
        events(&log, "turn.model")
            .iter()
            .any(|m| m["role"] == "preflight" && m["model"] == "wire-smart"),
        "preflight ran on the turn model: {log}"
    );
    let routed = events(&log, "turn.routed");
    assert_eq!(routed.len(), 1, "{log}");
    assert_eq!(routed[0]["model"], "deep", "{log}");
    let (reply, turns) = answered(&log);
    assert_eq!(reply, "answered on deep", "{log}");
    assert_eq!(turns, vec![json!("d-1")], "{log}");
    let _ = std::fs::remove_file(&c);
}

/// A decision model answers the route as one more choice, the routes'
/// criteria its options, and the turn runs on its choice.
#[test]
fn a_decision_model_preflight_routes_the_turn() {
    let c = chat(ENUM, json!("unused"));
    let dec = playbook(json!({"decisions": [{
        "when_contains": "a quick answer or a status question",
        "answers": {
            "intent": {"type": "choice", "choice": "question", "confidence": 0.9},
            "needs_plan": {"type": "noul", "noul": 0.05},
            "risk": {"type": "choice", "choice": "low", "confidence": 0.95},
            "route": {"type": "choice", "choice": "fast", "confidence": 0.92}
        }
    }]}));
    let log = run(&config(
        WORK,
        "always",
        &c,
        Some(&dec),
        "{ turn: smart, preflight: { model: jev, confidence: 0.7 } }",
        "",
    ));
    let verdict = events(&log, "preflight.verdict");
    assert_eq!(verdict.len(), 1, "{log}");
    assert_eq!(verdict[0]["route"], "fast", "{log}");
    assert_eq!(events(&log, "turn.routed")[0]["model"], "fast", "{log}");
    let (reply, turns) = answered(&log);
    assert_eq!(reply, "answered on fast", "{log}");
    assert_eq!(turns, vec![json!("f-1")], "{log}");
    for f in [&c, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// An unsure route is no route, not no verdict: the rest of the triage
/// stands and the turn runs on `roles.turn`. A preflight that fails
/// outright leaves no verdict, and the turn runs there too.
#[test]
fn an_unsure_or_failed_preflight_leaves_the_turn_on_the_turn_model() {
    let c = chat(LISTED, json!("not a verdict"));
    let dec = playbook(json!({"decisions": [{"answers": {
        "intent": {"type": "choice", "choice": "task", "confidence": 0.9},
        "needs_plan": {"type": "noul", "noul": 0.05},
        "risk": {"type": "choice", "choice": "low", "confidence": 0.95},
        "route": {"type": "choice", "choice": "deep", "confidence": 0.4}
    }}]}));
    let log = run(&config(
        WORK,
        "always",
        &c,
        Some(&dec),
        "{ turn: smart, preflight: { model: jev, confidence: 0.7 } }",
        "",
    ));
    let verdict = events(&log, "preflight.verdict");
    assert_eq!(verdict.len(), 1, "the rest of the triage stands: {log}");
    assert_eq!(verdict[0]["intent"], "task", "{log}");
    assert!(verdict[0]["route"].is_null(), "{log}");
    let unsure = events(&log, "preflight.route_unsure");
    assert_eq!(unsure.len(), 1, "{log}");
    assert_eq!(unsure[0]["choice"], "deep", "{log}");
    assert!(events(&log, "turn.routed").is_empty(), "{log}");
    let (reply, turns) = answered(&log);
    assert_eq!(reply, "answered on the turn model", "{log}");
    assert_eq!(turns, vec![json!("wire-smart")], "{log}");

    // The chat preflight answers no verdict at all: it fails, and the turn
    // runs on the turn model.
    let log = run(&config(WORK, "always", &c, None, "{ turn: smart }", ""));
    assert_eq!(events(&log, "preflight.failed").len(), 1, "{log}");
    assert!(events(&log, "turn.routed").is_empty(), "{log}");
    let (reply, turns) = answered(&log);
    assert_eq!(reply, "answered on the turn model", "{log}");
    assert_eq!(turns, vec![json!("wire-smart")], "{log}");
    for f in [&c, &dec] {
        let _ = std::fs::remove_file(f);
    }
}

/// Under `preflight: auto` only a message preflight looks at is routed: a
/// short remark with no work in it is answered on `roles.turn`.
#[test]
fn a_message_preflight_does_not_look_at_is_not_routed() {
    let c = chat(
        ENUM,
        json!({"intent": "task", "needs_plan": false, "risk": "low", "route": "deep"}),
    );
    let log = run(&config("hi there", "auto", &c, None, "{ turn: smart }", ""));
    assert!(events(&log, "preflight.start").is_empty(), "{log}");
    assert!(events(&log, "turn.routed").is_empty(), "{log}");
    let (reply, turns) = answered(&log);
    assert_eq!(reply, "answered on the turn model", "{log}");
    assert_eq!(turns, vec![json!("wire-smart")], "{log}");
    let _ = std::fs::remove_file(&c);
}

/// A connection asked in the prompt alone is sent no schema, so preflight's
/// prompt names the routes and their criteria — and the turn is routed all
/// the same.
#[test]
fn a_connection_asked_in_the_prompt_alone_is_told_the_routes() {
    let c = chat(
        LISTED,
        json!({"intent": "task", "needs_plan": false, "risk": "low", "route": "deep"}),
    );
    let log = run(&config(
        WORK,
        "always",
        &c,
        None,
        "{ turn: smart }",
        "  structured_output: prompt\n",
    ));
    assert_eq!(events(&log, "turn.routed")[0]["model"], "deep", "{log}");
    let (reply, turns) = answered(&log);
    assert_eq!(reply, "answered on deep", "{log}");
    assert_eq!(turns, vec![json!("d-1")], "{log}");
    let _ = std::fs::remove_file(&c);
}

/// Routes are the parent's per-turn policy and stay with it: an instance
/// child is composed without them, so a template that turns its preflight
/// off still spawns (routes with `preflight: never` would refuse the child).
#[cfg(feature = "a2a")]
#[test]
fn an_instance_child_is_composed_without_the_parents_routes() {
    let dir = common::unique_path("routing-instance", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        r#"agent: { name: parent }
store: { kind: memory }
lifecycle: { run_until: idle, idle_grace: 1500ms }
observability: { log_level: info, log_content: true }
intelligence:
  endpoints: "mock:final"
  models:
    smart: { model: mock }
    fast: { model: mock-fast }
  roles: { turn: smart }
  routes:
    fast: "a quick answer"
subagents:
  templates:
    room:
      instruction: |
        You are the room for {{params.id}}.
        :::!config
        agent: { preflight: never }
        :::
        :::!workflow{name=on-ping}
        steps:
          cmd: { kind: a2a, command: room.ping, roles: [agent, operator] }
          f:   { kind: finish, depends_on: [cmd], status: completed, output: "pong {{params.id}}" }
        :::
      params: { id: { type: string, required: true } }
      ttl: 4s
workflows:
  - name: caller
    steps:
      s:     { kind: once }
      spawn: { kind: subagent, template: room, params: { id: inc-7 }, depends_on: [s] }
      ask:   { kind: a2a.delegate, depends_on: [spawn], peer: "{{steps.spawn.output.peer}}", command: room.ping, args: { x: hello }, timeout: 10s, retry: { max: 3, backoff: 1s } }
      f:     { kind: finish, depends_on: [ask], status: completed, output: "{{steps.ask.output}}" }
"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(out.status.code(), Some(0), "{log}");
    let done = events(&log, "run.done");
    assert!(
        done.iter()
            .any(|d| d["workflow"] == "caller" && d["output"] == "pong inc-7"),
        "{log}"
    );
}

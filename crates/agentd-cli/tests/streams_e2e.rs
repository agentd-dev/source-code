// SPDX-License-Identifier: AGPL-3.0-only
//! Event streams (RFC 0035 Phase A), end to end: one workflow `emit`s domain
//! events, a DIFFERENT workflow's `stream` start consumes them — and, the
//! property no other edge has, a consumer that did not exist when the events
//! were published still processes them: life 1 emits with no consumer
//! configured; life 2 adds the consumer with `from: earliest` and the
//! backlog replays, in order, exactly once. Life 3 proves the durable
//! offset: nothing re-fires.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

use serde_json::Value;

fn events(stderr: &str, name: &str) -> Vec<Value> {
    stderr
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == name)
        .collect()
}

fn config(dir: &str, with_consumer: bool) -> String {
    let consumer = if with_consumer {
        "  - name: fulfil\n    steps:\n\
         \x20     take: {kind: stream, stream: orders, subject: \"order.*\", from: earliest}\n\
         \x20     note: {kind: assign, depends_on: [take], value: \"handled {{steps.take.output.subject}} #{{steps.take.output.data.n}}\"}\n\
         \x20     f:    {kind: finish, depends_on: [note], status: completed, output: \"{{steps.note.output}}\"}\n"
    } else {
        ""
    };
    format!(
        "config_version: \"1\"\nagent:\n  name: eventful\n\
         store:\n  kind: file\n  file:\n    path: {dir}/state\n  checkpoint:\n    debounce_ms: 0\n\
         streams:\n  orders:\n    retention: {{ max_events: 100 }}\n\
         workflows:\n  - name: producer\n    steps:\n\
         \x20     s:    {{kind: once, policy: always}}\n\
         \x20     each: {{kind: foreach, depends_on: [s], over: [1, 2, 3], batch: {{size: 1, parallel: 1}},\n\
         \x20            body: {{steps: {{pub: {{kind: emit, stream: orders, subject: \"order.paid\", correlation: \"o-{{{{item}}}}\", data: {{n: \"{{{{item}}}}\"}}}}}}}}}}\n\
         \x20     f:    {{kind: finish, depends_on: [each], status: completed}}\n{consumer}\
         lifecycle:\n  run_until: idle\n  idle_grace: 900ms\n\
         observability:\n  log_level: info\n  log_content: true\n"
    )
}

fn life(cfg: &str) -> (Option<i32>, String) {
    let err_path = common::unique_path("streams", "log");
    let errf = std::fs::File::create(&err_path).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(errf))
        .status()
        .expect("run");
    let log = std::fs::read_to_string(&err_path).unwrap_or_default();
    let _ = std::fs::remove_file(&err_path);
    (out.code(), log)
}

#[test]
fn another_workflows_events_replay_into_a_late_consumer_exactly_once() {
    let dir = common::unique_path("streams", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");

    // Life 1: the producer emits three events. No consumer exists yet.
    std::fs::write(&cfg, config(&dir, false)).unwrap();
    let (code, l1) = life(&cfg);
    assert_eq!(code, Some(0), "{l1}");
    assert_eq!(events(&l1, "stream.emit").len(), 3, "{l1}");

    // Life 2: the consumer arrives with `from: earliest` — the backlog it
    // never saw published replays through it. (The producer also runs again,
    // adding three MORE events, consumed live in the same life.)
    std::fs::write(&cfg, config(&dir, true)).unwrap();
    let (code, l2) = life(&cfg);
    assert_eq!(code, Some(0), "{l2}");
    let done: Vec<String> = events(&l2, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "fulfil" && e["status"] == "completed")
        .filter_map(|e| e["output"].as_str().map(str::to_string))
        .collect();
    assert_eq!(
        done.len(),
        6,
        "3 replayed + 3 live, exactly once each:\n{l2}"
    );
    assert_eq!(
        done.iter().filter(|o| o.contains("#1")).count(),
        2,
        "each item once per producer life:\n{done:?}"
    );
    assert!(
        done.iter().all(|o| o.starts_with("handled order.paid")),
        "{done:?}"
    );

    // Life 3: producer `once` fires again (3 new events, consumed), but the
    // six ALREADY-consumed events never re-fire — the offset is durable.
    std::fs::write(&cfg, config(&dir, true)).unwrap();
    let (code, l3) = life(&cfg);
    assert_eq!(code, Some(0), "{l3}");
    let done3 = events(&l3, "run.done")
        .iter()
        .filter(|e| e["workflow"] == "fulfil" && e["status"] == "completed")
        .count();
    assert_eq!(done3, 3, "only THIS life's events fire:\n{l3}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_undeclared_stream_is_refused_at_startup() {
    let dir = common::unique_path("streams-bad", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(
        &cfg,
        "config_version: \"1\"\nagent:\n  name: x\nstore:\n  kind: memory\n\
         workflows:\n  - name: w\n    steps:\n\
         \x20     s: {kind: once}\n\
         \x20     e: {kind: emit, depends_on: [s], stream: nope, subject: a.b}\n\
         \x20     f: {kind: finish, depends_on: [e]}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .output()
        .expect("run");
    assert_ne!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The refusal reaches stderr inside a JSON log line, so the inner quotes
    // arrive escaped — match around them.
    assert!(
        stderr.contains("nope") && stderr.contains("is not declared under"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

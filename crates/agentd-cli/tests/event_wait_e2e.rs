// SPDX-License-Identifier: AGPL-3.0-only
//! `wait {on: event}`: a run parks on the durable log until an event matching
//! a subject and a predicate arrives.
//!
//! The two properties worth testing are the ones no other wait has:
//! `match` sees THIS run's inputs beside the event (so correlation is
//! expressible at all), and a timeout makes ABSENCE a declared branch.
#![cfg(all(unix, feature = "workflow", feature = "cel"))]

mod common;

use std::process::{Command, Stdio};

fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("evwait", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = format!("{dir}/c.yaml");
    std::fs::write(&cfg, cfg_text.replace("__STATE__", &format!("{dir}/state"))).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .args(["--config", &cfg])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .expect("run");
    let log = String::from_utf8_lossy(&out.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (out.status.code(), log)
}

const BASE: &str = "config_version: \"1\"\nagent: { name: e }\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info, log_content: true }\n\
     lifecycle: { run_until: idle, idle_grace: 3s }\n\
     streams: { orders: { retention: { max_events: 100 } } }\n";

/// The saga shape, in one graph instead of two workflows plus bookkeeping:
/// this run emits, then parks until the matching reply lands.
#[test]
fn a_run_parks_on_the_log_and_resumes_on_a_matching_event() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: saga\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     paid: {{ kind: emit, stream: orders, subject: order.paid, data: {{ id: A1 }}, depends_on: [s] }}\n\
        \x20     settle: {{ kind: sleep, duration: 400ms, depends_on: [paid] }}\n\
        \x20     ship: {{ kind: emit, stream: orders, subject: order.shipped, data: {{ id: A1 }}, depends_on: [settle] }}\n\
        \x20     await: {{ kind: wait, on: event, stream: orders, subject: order.shipped, timeout: 5s, depends_on: [paid] }}\n\
        \x20     f: {{ kind: finish, depends_on: [await], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"kind\":\"event\"") && log.contains("\"event\":\"wait.resolved\""),
        "the wait should have resolved on the event\n{log}"
    );
    assert!(
        !log.contains("\"status\":\"timeout\""),
        "it should resolve on arrival, not time out\n{log}"
    );
}

/// The reason this node exists. A `stream` START's filter sees only the event;
/// there was nowhere in the language to say "the one for the order THIS run is
/// about". `match` sees `inputs` beside `event`, so a run ignores the traffic
/// meant for its siblings.
#[test]
fn match_can_correlate_an_event_against_this_runs_own_inputs() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: corr\n    steps:\n\
        \x20     s: {{ kind: once, inputs: {{ order_id: A1 }} }}\n\
        \x20     settle: {{ kind: sleep, duration: 400ms, depends_on: [s] }}\n\
        \x20     other: {{ kind: emit, stream: orders, subject: order.shipped, data: {{ id: ZZ }}, depends_on: [settle] }}\n\
        \x20     mine: {{ kind: emit, stream: orders, subject: order.shipped, data: {{ id: A1 }}, depends_on: [other] }}\n\
        \x20     await:\n        kind: wait\n        on: event\n        stream: orders\n        subject: order.shipped\n\
        \x20       match: \"CEL: event.data.id == inputs.order_id\"\n        timeout: 5s\n        depends_on: [s]\n\
        \x20     f: {{ kind: finish, depends_on: [await], status: completed, output: \"{{{{ steps.await.output.data.id }}}}\" }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    // It must skip the ZZ event and resolve on A1 — the whole point.
    assert!(
        log.contains("\"output\":\"A1\""),
        "the wait should have resolved on the correlated event, not the first one\n{log}"
    );
}

/// Absence as a declared branch: nothing arrives, the timeout fires, and
/// `on_timeout` routes it — no polling loop, no synthetic schedule.
#[test]
fn an_event_that_never_arrives_routes_through_on_timeout() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: absent\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     await: {{ kind: wait, on: event, stream: orders, subject: order.shipped, timeout: 700ms, on_timeout: escalate, depends_on: [s] }}\n\
        \x20     escalate: {{ kind: assign, value: paged }}\n\
        \x20     f: {{ kind: finish, depends_on: [escalate], status: completed, output: escalated }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"output\":\"escalated\""),
        "absence should have taken the on_timeout branch\n{log}"
    );
}

/// Anchored at arm time. A wait must not resolve on an event that predates it:
/// the step would "succeed" on work nobody asked for, and its idempotency key
/// would cover a different world. There is deliberately no `from: earliest`.
#[test]
fn a_wait_never_resolves_on_an_event_that_predates_it() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: anchored\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     old: {{ kind: emit, stream: orders, subject: order.shipped, data: {{ id: OLD }}, depends_on: [s] }}\n\
        \x20     await: {{ kind: wait, on: event, stream: orders, subject: order.shipped, timeout: 700ms, on_timeout: late, depends_on: [old] }}\n\
        \x20     late: {{ kind: assign, value: nothing-new }}\n\
        \x20     f: {{ kind: finish, depends_on: [late], status: completed, output: \"{{{{ steps.late.output }}}}\" }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"output\":\"nothing-new\""),
        "the already-emitted event must not satisfy a wait armed after it\n{log}"
    );
}

/// An undeclared stream is a configuration mistake, and the step says so
/// rather than parking forever on a log that will never exist.
#[test]
fn waiting_on_an_undeclared_stream_fails_the_step() {
    let (_code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: bad\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     await: {{ kind: wait, on: event, stream: nope, timeout: 1s, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [await], status: completed }}\n"
    ));
    assert!(
        log.contains("is not declared"),
        "an undeclared stream should fail the step at arm time\n{log}"
    );
}

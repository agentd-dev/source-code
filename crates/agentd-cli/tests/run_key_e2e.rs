// SPDX-License-Identifier: AGPL-3.0-only
//! `key:` — a run's logical identity, and `concurrency.scope: key`.
//!
//! Everything else in the runtime is keyed: breakers, rate buckets, start
//! state, webhook dedup, step idempotency. The run had only an id, which is
//! why "never two runs for the same customer at once" was not expressible —
//! `max_runs` could count runs, but not runs *about the same account*.
//!
//! The distinction under test is the one that matters: `scope: workflow` with
//! `max_runs: 1` is a QUEUE (every entity behind one run); `scope: key` is a
//! per-entity LOCK (each entity serialised against itself, entities parallel).
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("runkey", "d");
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

const BASE: &str = "config_version: \"1\"\nagent: { name: k }\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info, log_content: true }\n\
     lifecycle: { run_until: idle, idle_grace: 3s }\n\
     streams: { orders: { retention: { max_events: 100 } } }\n";

/// Two events about DIFFERENT entities run concurrently under `max_runs: 1`,
/// because the cap counts per key rather than per workflow. Under
/// `scope: workflow` the same config would serialise them.
#[test]
fn different_keys_are_not_serialised_behind_each_other() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: feeder\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     a: {{ kind: emit, stream: orders, subject: sync, data: {{ account: A }}, depends_on: [s] }}\n\
        \x20     b: {{ kind: emit, stream: orders, subject: sync, data: {{ account: B }}, depends_on: [a] }}\n\
        \x20     f: {{ kind: finish, depends_on: [b], status: completed }}\n\
        \x20 - name: sync\n\
        \x20   key: \"{{{{ payload.data.account }}}}\"\n\
        \x20   concurrency: {{ max_runs: 1, scope: key }}\n    steps:\n\
        \x20     s: {{ kind: stream, stream: orders, subject: sync }}\n\
        \x20     work: {{ kind: sleep, duration: 600ms, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [work], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    // Both entities got their own run rather than one waiting on the other.
    assert!(
        log.matches("\"event\":\"run.start\"").count() >= 3,
        "both keyed runs plus the feeder should have started\n{log}"
    );
    assert!(
        !log.contains("\"reason\":\"concurrency\""),
        "different keys must not contend\n{log}"
    );
}

/// `scope: key` without a `key:` template would silently put every run in one
/// bucket — the opposite of what was asked for, and invisible until two
/// entities collided in production. It is a load error.
#[test]
fn keyed_concurrency_without_a_key_template_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: bad\n    concurrency: {{ max_runs: 1, scope: key }}\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("needs a `key:` template"),
        "the refusal should say what is missing\n{log}"
    );
}

/// The key is rendered from the trigger payload and recorded on the run, so a
/// restart still knows which runs are about the same entity.
#[test]
fn the_key_is_rendered_from_the_trigger_payload() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: feeder\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     a: {{ kind: emit, stream: orders, subject: sync, data: {{ account: ACME }}, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [a], status: completed }}\n\
        \x20 - name: sync\n\
        \x20   key: \"{{{{ payload.data.account }}}}\"\n\
        \x20   concurrency: {{ max_runs: 2, scope: key }}\n    steps:\n\
        \x20     s: {{ kind: stream, stream: orders, subject: sync }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"key\":\"ACME\""),
        "the rendered key should be recorded on the run\n{log}"
    );
}

/// An unknown scope is a config error, not a silently-ignored word.
#[test]
fn an_unknown_concurrency_scope_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}workflows:\n\
        \x20 - name: bad\n    key: \"x\"\n    concurrency: {{ scope: entity }}\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("must be workflow|key"), "{log}");
}

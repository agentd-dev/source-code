// SPDX-License-Identifier: AGPL-3.0-only
//! The runtime-events stream: the daemon's own event vocabulary, appended to a
//! declared stream so a workflow can react to it.
//!
//! What matters here is that the loop actually closes — a runtime event starts
//! a run — and that the guards hold: undeclared streams and unknown families
//! are startup errors, and the tap cannot feed itself.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("rtev", "d");
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

const BASE: &str = "config_version: \"1\"\nagent: { name: r }\n\
     store: { kind: file, file: { path: __STATE__ } }\n";

/// The loop closes: a run finishing emits `run.done`, the tap appends it to
/// `_runtime`, and a `stream` start consuming `_runtime` fires a second run.
/// The daemon reacted to itself, which it could not do before.
#[test]
fn a_runtime_event_can_start_a_run() {
    let (code, log) = run(&format!(
        "{BASE}lifecycle: {{ run_until: idle, idle_grace: 3s }}\n\
         streams: {{ _runtime: {{ retention: {{ max_events: 500 }} }} }}\n\
         observability:\n  log_level: info\n\
        \x20 runtime_events: {{ stream: _runtime, include: [run] }}\n\
         workflows:\n\
        \x20 - name: work\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed, output: did-work }}\n\
        \x20 - name: watcher\n    steps:\n\
        \x20     s: {{ kind: stream, stream: _runtime, subject: \"run.done\" }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed, output: reacted }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"event\":\"stream.tap\""),
        "the tap should have armed at startup\n{log}"
    );
    // The watcher exists only to consume `_runtime`, so a run of it is proof
    // the daemon reacted to its own telemetry.
    assert!(
        log.contains("\"workflow\":\"watcher\"") && log.contains("\"event\":\"run.start\""),
        "a runtime event should have started the watcher run\n{log}"
    );
}

/// Selection is per family, so an instance that only wants breaker and
/// pressure telemetry does not durably record every turn it takes.
#[test]
fn families_not_included_never_reach_the_stream() {
    let (code, log) = run(&format!(
        "{BASE}lifecycle: {{ run_until: idle, idle_grace: 2s }}\n\
         streams: {{ _runtime: {{ retention: {{ max_events: 500 }} }} }}\n\
         observability:\n  log_level: info\n\
        \x20 runtime_events: {{ stream: _runtime, include: [breaker] }}\n\
         workflows:\n\
        \x20 - name: work\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed, output: done }}\n\
        \x20 - name: watcher\n    steps:\n\
        \x20     s: {{ kind: stream, stream: _runtime, subject: \"run.done\" }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed, output: reacted }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !log.contains("\"workflow\":\"watcher\",\"...\"") && !log.contains("\"run\":\"watcher-"),
        "run.* was not included, so nothing should have consumed it\n{log}"
    );
}

/// Fail-closed at boot. A stream that is not declared can never be appended
/// to, so the config is refused where an operator is looking rather than
/// failing silently inside a storm later.
#[test]
fn an_undeclared_stream_is_refused_at_startup() {
    let (code, log) = run(&format!(
        "{BASE}lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         observability:\n  log_level: info\n\
        \x20 runtime_events: {{ stream: nope, include: [run] }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("is not declared"),
        "the refusal should name the missing stream\n{log}"
    );
}

/// A typo in a family name would otherwise be a filter that silently matches
/// nothing — the shipped-config-that-lies failure mode. It is an error.
#[test]
fn an_unknown_family_is_refused_at_startup() {
    let (code, log) = run(&format!(
        "{BASE}lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         streams: {{ _runtime: {{ retention: {{ max_events: 10 }} }} }}\n\
         observability:\n  log_level: info\n\
        \x20 runtime_events: {{ stream: _runtime, include: [pressur] }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("unknown event family"),
        "the refusal should name the typo\n{log}"
    );
}

/// The tap must not observe its own plumbing. A stream held at a tiny
/// retention trims on nearly every append and logs `stream.trimmed` each
/// time; if that line were itself tapped, one appended event would produce the
/// next one forever, fastest exactly when the stream is already full.
#[test]
fn the_tap_does_not_feed_itself() {
    let (code, log) = run(&format!(
        "{BASE}lifecycle: {{ run_until: idle, idle_grace: 3s }}\n\
         streams: {{ _runtime: {{ retention: {{ max_events: 2 }} }} }}\n\
         observability:\n  log_level: info\n\
        \x20 runtime_events: {{ stream: _runtime, include: [stream, run] }}\n\
         workflows:\n\
        \x20 - name: work\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     f: {{ kind: finish, depends_on: [s], status: completed, output: done }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    // A self-feeding tap shows up as unbounded trimming. A handful is normal
    // for a 2-event retention; hundreds would mean the loop closed.
    let trims = log.matches("\"event\":\"stream.trimmed\"").count();
    assert!(
        trims < 100,
        "runaway trimming ({trims}) suggests the tap is appending its own output\n{log}"
    );
}

/// Audit records were written and then unreadable: `Kind::Audit` is
/// deliberately not manifest-indexed, so nothing could list them back. The
/// stream sink is the supported way to read them — and it needs its stream.
#[test]
fn the_audit_stream_sink_requires_a_declared_stream() {
    let (code, log) = run(&format!(
        "{BASE}lifecycle: {{ run_until: idle, idle_grace: 1s }}\n\
         observability:\n  log_level: info\n  audit: {{ sink: [stream] }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("needs `stream:"),
        "the refusal should say what is missing\n{log}"
    );
}

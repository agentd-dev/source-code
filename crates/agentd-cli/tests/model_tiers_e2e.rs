// SPDX-License-Identifier: AGPL-3.0-only
//! `intelligence.models`: the model as a named tier rather than one
//! instance-global string.
//!
//! Choosing a cheap model for a classify step and a frontier one for a
//! judgement call meant forking a subagent process just to change a string —
//! `model` appeared in no node's field list, so writing it on a step was
//! exit 2. And the breaker was per ENDPOINT, so a frontier and a cheap model
//! behind one gateway shared one breaker and one spend pool.
//!
//! A tier is NOT a second service catalogue: it points at a `services:` entry
//! and may only narrow, inheriting that service's trifecta tags so "make it
//! cheaper" cannot quietly become a different security decision.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("tiers", "d");
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

const BASE: &str = "config_version: \"1\"\nagent: { name: t }\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info, log_content: true }\n\
     lifecycle: { run_until: idle, idle_grace: 2s }\n";

const TIERS: &str = "intelligence:\n  endpoints: \"mock:json\"\n\
    \x20 models:\n\
    \x20   big:   { model: big-model-1, window: 200000, fallback: small }\n\
    \x20   small: { model: small-model-3, window: 128000 }\n\
    \x20 default: big\n";

/// A step naming a tier runs on that tier's wire model — cost tiering inside
/// one workflow, with no second process.
#[test]
fn a_step_can_name_a_cheaper_tier_than_the_instance_default() {
    let (code, log) = run(&format!(
        "{BASE}{TIERS}workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     t: {{ kind: think, prompt: \"classify this\", model: small, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [t], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"model\":\"small-model-3\""),
        "the step should have run on the tier's wire model\n{log}"
    );
}

/// The instance default resolves through the catalogue too, so `default: big`
/// means the wire name, not the tier name, reaches the provider.
#[test]
fn the_default_tier_resolves_to_its_wire_model() {
    let (code, log) = run(&format!(
        "{BASE}{TIERS}workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     t: {{ kind: think, prompt: \"decide\", depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [t], status: completed }}\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"model\":\"big-model-1\"") && !log.contains("\"model\":\"big\""),
        "the tier NAME must not reach the provider\n{log}"
    );
}

/// A typo in a tier name is a startup error, not a run that quietly asks the
/// provider for a model called `smal`.
#[test]
fn an_unknown_tier_on_a_step_is_refused_at_startup() {
    let (code, log) = run(&format!(
        "{BASE}{TIERS}workflows:\n\
        \x20 - name: w\n    steps:\n\
        \x20     s: {{ kind: once }}\n\
        \x20     t: {{ kind: think, prompt: \"x\", model: smal, depends_on: [s] }}\n\
        \x20     f: {{ kind: finish, depends_on: [t], status: completed }}\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("is not a declared tier"), "{log}");
}

/// A degradation ladder that loops is a hang under exactly the conditions it
/// exists to survive.
#[test]
fn a_fallback_cycle_is_refused_at_startup() {
    let (code, log) = run(&format!(
        "{BASE}intelligence:\n  endpoints: \"mock:json\"\n\
        \x20 models:\n\
        \x20   a: {{ model: m-a, fallback: b }}\n\
        \x20   b: {{ model: m-b, fallback: a }}\n\
        \x20 default: a\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("fallback cycle"), "{log}");
}

/// A tier pointing at a service that is not `kind: intelligence` is a
/// configuration mistake — it would inherit the wrong tags.
#[test]
fn a_tier_pointing_at_a_non_intelligence_service_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}services:\n  billing: {{ kind: mcp, endpoint: \"https://b.example/mcp\" }}\n\
         intelligence:\n  endpoints: \"mock:json\"\n\
        \x20 models:\n    x: {{ model: m, service: billing }}\n  default: x\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(
        log.contains("needs `kind: intelligence`"),
        "the refusal should name the mismatch\n{log}"
    );
}

/// A tier's declared window replaces the guess from the model NAME — a
/// substring match that is simply wrong for any provider whose naming does
/// not happen to match.
#[test]
fn a_declared_window_replaces_the_guess_from_the_model_name() {
    let (code, log) = run(&format!(
        "{BASE}intelligence:\n  endpoints: \"mock:json\"\n\
        \x20 models:\n    only: {{ model: entirely-unrecognisable-name, window: 7777 }}\n\
        \x20 default: only\n"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        !log.contains("config.invalid"),
        "a declared window should be accepted\n{log}"
    );
}

/// An unknown tier named by `default` is refused rather than treated as a
/// literal that happens to look like a tier name.
#[test]
fn an_unknown_default_tier_is_refused() {
    let (code, log) = run(&format!(
        "{BASE}intelligence:\n  endpoints: \"mock:json\"\n\
        \x20 models:\n    small: {{ model: m-1 }}\n  default: enormous\n"
    ));
    assert_eq!(code, Some(2), "{log}");
    assert!(log.contains("not a declared model tier"), "{log}");
}

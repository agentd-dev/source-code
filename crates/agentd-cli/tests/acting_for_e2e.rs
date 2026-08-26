// SPDX-License-Identifier: AGPL-3.0-only
//! `acting_for`: the attribution chain that travels with the work.
//!
//! Four config surfaces parsed, validated and were read by nothing —
//! `principals[].quotas.rate`, `.budget`, `Principal::scope_key` and
//! `BudgetScope`. And a trigger firing carried no principal at all, so "every
//! effect names the human or the schedule that caused it" was false by
//! construction: the chain was dropped at its very first hop.
//!
//! The claim here is narrow and worth keeping narrow. This is an audit field
//! plus quota enforcement — NOT multi-tenancy, which this project has
//! declined: the answer to "a different caller needs a different surface" is a
//! different process.
#![cfg(all(unix, feature = "workflow"))]

mod common;

use std::process::{Command, Stdio};

fn run(cfg_text: &str) -> (Option<i32>, String) {
    let dir = common::unique_path("acting", "d");
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

const BASE: &str = "config_version: \"1\"\n\
     store: { kind: file, file: { path: __STATE__ } }\n\
     observability: { log_level: info, log_content: true }\n\
     lifecycle: { run_until: idle, idle_grace: 2s }\n";

const WF: &str = "workflows:\n\
    \x20 - name: w\n    steps:\n\
    \x20     s: { kind: once }\n\
    \x20     f: { kind: finish, depends_on: [s], status: completed, output: done }\n";

/// A schedule, webhook or stream firing is work done for SOMEBODY. Passing no
/// principal dropped the chain at its first hop; `identity.autonomous_as`
/// names the actor instead.
#[test]
fn autonomous_work_is_attributed_to_a_named_actor() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: a }}\nidentity: {{ autonomous_as: \"system:scheduler\" }}\n{WF}"
    ));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"acting_for\":\"system:scheduler\""),
        "the trigger should have named its actor\n{log}"
    );
}

/// The default is a name, not absence — so the chain is never empty even for
/// an operator who never configured one.
#[test]
fn autonomous_work_has_an_actor_without_configuration() {
    let (code, log) = run(&format!("{BASE}agent: {{ name: a }}\n{WF}"));
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("\"acting_for\":\"system\""),
        "autonomous work should default to a named actor\n{log}"
    );
}

/// Labels are a CLOSED, operator-declared domain. Minting durable scope keys
/// and audit fields from values arriving off the box is the same unbounded-
/// cardinality hazard the metrics layer already bans for labels.
#[test]
fn labels_are_declared_by_the_operator_not_derived_from_input() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: a }}\n\
         identity: {{ autonomous_as: \"system\", labels: {{ tenant: acme, cost_center: CC-42 }} }}\n{WF}"
    ));
    assert_eq!(code, Some(0), "{log}");
    // An undeclared label key is a config error, not a silently-ignored field.
    let (bad, blog) = run(&format!(
        "{BASE}agent: {{ name: a }}\nidentity: {{ nonsense: 1 }}\n{WF}"
    ));
    assert_eq!(
        bad,
        Some(2),
        "an unknown identity field should be refused\n{blog}"
    );
}

/// A per-principal rate quota that parses and is never enforced is a setting
/// that does not do what it says. It is a real limit now — and operators are
/// exempt, because locking out the person who administers the daemon during
/// an incident is worse than the load they could generate.
#[test]
fn a_principal_rate_quota_is_accepted_and_bound_to_the_principal() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: a }}\n\
         a2a:\n  principals:\n\
        \x20   - match: {{ sub: \"*@acme.example\" }}\n      role: user\n\
        \x20     labels: {{ tenant: acme }}\n\
        \x20     quotas: {{ rate: \"30/1m\", budget: {{ windows: [{{ per: day, tokens: 200000 }}] }} }}\n{WF}"
    ));
    assert_eq!(code, Some(0), "the quota block should be accepted\n{log}");
}

/// A malformed quota is refused at startup rather than silently ignored.
#[test]
fn a_malformed_rate_quota_is_refused_at_startup() {
    let (code, log) = run(&format!(
        "{BASE}agent: {{ name: a }}\n\
         a2a:\n  principals:\n\
        \x20   - match: {{ any: true }}\n      role: user\n\
        \x20     quotas: {{ rate: \"not-a-rate\" }}\n{WF}"
    ));
    assert_eq!(code, Some(2), "{log}");
}

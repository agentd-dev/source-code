// SPDX-License-Identifier: Apache-2.0
//! The conformance suite as `cargo test`: one test per family, each running its
//! checks against a freshly-built agentd and asserting every check passes. The
//! same checks back the `agentd-conformance` runner binary.

use agentd_conformance::{Check, Harness, checks, run_check};

fn run_family(name: &str, family: Vec<Check>) {
    let h = Harness::new();
    let mut failures = Vec::new();
    for c in &family {
        let o = run_check(&h, c);
        if !o.passed {
            failures.push(format!("  {}: {}", c.id, o.detail));
        }
    }
    assert!(
        failures.is_empty(),
        "{} conformance failures ({}/{}):\n{}",
        name,
        failures.len(),
        family.len(),
        failures.join("\n")
    );
}

#[test]
fn supervisor_conformance() {
    run_family("supervisor", checks::supervisor::checks());
}

#[test]
fn security_conformance() {
    run_family("security", checks::security::checks());
}

#[test]
fn store_conformance() {
    run_family("store", checks::store::checks());
}

#[test]
fn durability_conformance() {
    run_family("durability", checks::durability::checks());
}

#[test]
fn tools_conformance() {
    run_family("tools", checks::tools::checks());
}

#[test]
fn a2a_conversation_conformance() {
    run_family("a2a-conversation", checks::a2a_conversation::checks());
}

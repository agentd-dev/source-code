// SPDX-License-Identifier: AGPL-3.0-only
//! The conformance suite as `cargo test`: one test per family, each running its
//! checks against a freshly-built agentd and asserting every check passes. The
//! same checks back the `agentd-conformance` runner binary.

use agentd_conformance::{Category, Harness, family, run_check};

fn run_family(category: Category) {
    let checks = family(category);
    let h = Harness::new();
    let mut failures = Vec::new();
    for c in &checks {
        let o = run_check(&h, c);
        if !o.passed {
            failures.push(format!("  {}: {}", c.id, o.detail));
        }
    }
    assert!(
        failures.is_empty(),
        "{} conformance failures ({}/{}):\n{}",
        category.as_str(),
        failures.len(),
        checks.len(),
        failures.join("\n")
    );
}

#[test]
fn supervisor_conformance() {
    run_family(Category::Supervisor);
}

#[test]
fn security_conformance() {
    run_family(Category::Security);
}

#[test]
fn store_conformance() {
    run_family(Category::Store);
}

#[test]
fn durability_conformance() {
    run_family(Category::Durability);
}

#[test]
fn tools_conformance() {
    run_family(Category::Tools);
}

#[test]
fn a2a_conversation_conformance() {
    run_family(Category::A2aConversation);
}

#[test]
fn events_conformance() {
    run_family(Category::Events);
}

#[test]
fn extensions_conformance() {
    run_family(Category::Extensions);
}

#[test]
fn auth_conformance() {
    run_family(Category::Auth);
}

/// Every family runs as a test above: one missing here would never run under
/// `cargo test`, and CI would stay green over it.
#[test]
fn every_family_is_a_test() {
    let this = include_str!("conformance.rs");
    for c in Category::ALL {
        let call = format!("run_family(Category::{c:?});");
        assert_eq!(
            this.matches(&call).count(),
            1,
            "{} has no #[test] of its own here",
            c.as_str()
        );
    }
}

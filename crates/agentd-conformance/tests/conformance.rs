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
fn mcp_server_conformance() {
    run_family("mcp-server", checks::mcp_server::checks());
}

#[test]
fn mcp_client_conformance() {
    run_family("mcp-client", checks::mcp_client::checks());
}

#[test]
fn supervisor_conformance() {
    run_family("supervisor", checks::supervisor::checks());
}

#[test]
fn agent_loop_conformance() {
    run_family("agent-loop", checks::agent_loop::checks());
}

#[test]
fn security_conformance() {
    run_family("security", checks::security::checks());
}

#[test]
fn work_claim_conformance() {
    run_family("work-claim", checks::work_claim::checks());
}
